//! Native account client. The website owns identity; no database secrets live here.
//! Refresh credentials live only in the OS credential store. Access tokens are memory-only.
use anyhow::{Context, bail, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::{TryRng, rngs::SysRng};
use reqwest::{Client, StatusCode, Url};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, watch};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct User {
    pub id: String,
    pub email: String,
    pub name: String,
    pub image: Option<String>,
}

/// Cached identity is available offline; it is not proof of server authorization.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Status {
    #[default]
    SignedOut,
    SignedIn(User),
}

#[derive(Clone, Deserialize, Serialize)]
struct Saved {
    refresh_token: String,
    user: User,
    #[serde(default)]
    pending_logout: bool,
}

trait CredentialStore: Send + Sync {
    fn load(&self) -> anyhow::Result<Option<String>>;
    fn save(&self, value: &str) -> anyhow::Result<()>;
    fn clear(&self) -> anyhow::Result<()>;
}

struct KeyringStore(keyring::Entry);
impl CredentialStore for KeyringStore {
    fn load(&self) -> anyhow::Result<Option<String>> {
        match self.0.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(_) => bail!("Could not read account credentials from the OS credential store"),
        }
    }
    fn save(&self, value: &str) -> anyhow::Result<()> {
        self.0.set_password(value).map_err(|_| {
            anyhow::anyhow!("Could not save account credentials to the OS credential store")
        })
    }
    fn clear(&self) -> anyhow::Result<()> {
        match self.0.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(_) => bail!("Could not remove account credentials from the OS credential store"),
        }
    }
}

struct State {
    saved: Option<Saved>,
    access: Option<(String, Instant)>,
    generation: u64,
}

/// Clone this handle to use from background tasks. Mutations and refreshes are serialized.
#[derive(Clone)]
pub struct Account {
    inner: Arc<Inner>,
}
struct Inner {
    base: Url,
    client: Client,
    store: Arc<dyn CredentialStore>,
    state: Mutex<State>,
    status: watch::Sender<Status>,
}

/// In-progress login secrets intentionally do not implement Debug or Serialize.
pub struct PendingLogin {
    pub verification_uri: Url,
    device_code: String,
    verifier: String,
    deadline: Instant,
    interval: Duration,
    generation: u64,
    owner: Arc<Inner>,
}

#[derive(Deserialize)]
struct StartResponse {
    device_code: String,
    verification_uri: String,
    expires_in: u64,
    interval: u64,
}
#[derive(Deserialize)]
struct Tokens {
    token_type: String,
    access_token: String,
    refresh_token: String,
    expires_in: u64,
    user: User,
}
#[derive(Deserialize)]
struct ApiError {
    error: String,
}

impl Account {
    /// Accepts HTTPS origins, or HTTP loopback origins for local development only.
    /// Credential entries are scoped to the full origin, including development port.
    pub async fn new(origin: &str) -> anyhow::Result<Self> {
        let base = validate_origin(origin)?;
        let entry = keyring::Entry::new("bono.account", &base.origin().ascii_serialization())
            .map_err(|_| anyhow::anyhow!("Could not open OS credential store"))?;
        Self::with_store(base, Arc::new(KeyringStore(entry))).await
    }

    async fn with_store(base: Url, store: Arc<dyn CredentialStore>) -> anyhow::Result<Self> {
        let reader = store.clone();
        let saved = tokio::task::spawn_blocking(move || reader.load())
            .await??
            .map(|value| {
                serde_json::from_str::<Saved>(&value)
                    .map_err(|_| anyhow::anyhow!("Invalid saved account credentials"))
            })
            .transpose()?;
        let status = saved
            .as_ref()
            .filter(|s| !s.pending_logout)
            .map(|s| Status::SignedIn(s.user.clone()))
            .unwrap_or_default();
        let (status, _) = watch::channel(status);
        Ok(Self {
            inner: Arc::new(Inner {
                base,
                store,
                status,
                client: Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .timeout(Duration::from_secs(30))
                    .build()?,
                state: Mutex::new(State {
                    saved,
                    access: None,
                    generation: 0,
                }),
            }),
        })
    }

    pub fn subscribe(&self) -> watch::Receiver<Status> {
        self.inner.status.subscribe()
    }
    pub fn status(&self) -> Status {
        self.inner.status.borrow().clone()
    }

    fn endpoint(&self, name: &str) -> Url {
        self.inner
            .base
            .join(&format!("/api/desktop/auth/{name}"))
            .expect("fixed API path")
    }

    /// Prepare login without opening a window. A future UI can display/open the URL.
    pub async fn begin_login(&self) -> anyhow::Result<PendingLogin> {
        let mut state = self.inner.state.lock().await;
        if state.saved.as_ref().is_some_and(|s| s.pending_logout) {
            self.revoke_locked(&mut state).await?;
        }
        ensure!(
            state.saved.is_none(),
            "Log out before signing in to another account"
        );
        state.generation += 1;
        let verifier = new_verifier()?;
        let response = self.inner.client.post(self.endpoint("start"))
            .json(&serde_json::json!({"code_challenge": challenge(&verifier), "code_challenge_method": "S256"}))
            .send().await.context("Could not start account login")?;
        ensure!(
            response.status().is_success(),
            "Account login could not be started ({})",
            response.status()
        );
        let start: StartResponse = response.json().await.context("Invalid login response")?;
        let verification_uri = Url::parse(&start.verification_uri)?;
        ensure!(
            verification_uri.origin() == self.inner.base.origin()
                && verification_uri.username().is_empty()
                && verification_uri.password().is_none(),
            "Untrusted login URL"
        );
        ensure!(
            !start.device_code.is_empty()
                && (1..=1800).contains(&start.expires_in)
                && (1..=60).contains(&start.interval),
            "Invalid login parameters"
        );
        Ok(PendingLogin {
            verification_uri,
            device_code: start.device_code,
            verifier,
            deadline: Instant::now() + Duration::from_secs(start.expires_in),
            interval: Duration::from_secs(start.interval),
            generation: state.generation,
            owner: self.inner.clone(),
        })
    }

    /// System-browser login, suitable for a background task; no desktop UI changes.
    pub async fn login(&self) -> anyhow::Result<User> {
        let pending = self.begin_login().await?;
        webbrowser::open(pending.verification_uri.as_str())
            .context("Could not open sign-in in your browser")?;
        self.complete_login(pending).await
    }

    /// Poll until browser approval. Dropping this future cancels local polling.
    pub async fn complete_login(&self, mut pending: PendingLogin) -> anyhow::Result<User> {
        ensure!(
            Arc::ptr_eq(&pending.owner, &self.inner),
            "Login belongs to a different account client"
        );
        loop {
            ensure!(Instant::now() < pending.deadline, "Login request expired");
            tokio::time::sleep(pending.interval).await;
            let mut state = self.inner.state.lock().await;
            ensure!(
                state.generation == pending.generation,
                "Login was cancelled"
            );
            ensure!(Instant::now() < pending.deadline, "Login request expired");
            let response = self.inner.client.post(self.endpoint("token"))
                .json(&serde_json::json!({"device_code": pending.device_code, "code_verifier": pending.verifier}))
                .send().await.context("Could not complete account login")?;
            let status = response.status();
            if status.is_success() {
                let tokens: Tokens = response.json().await.context("Invalid token response")?;
                return self.install(&mut state, tokens).await;
            }
            if status == StatusCode::TOO_MANY_REQUESTS {
                pending.interval =
                    (pending.interval + Duration::from_secs(5)).min(Duration::from_secs(60));
                continue;
            }
            let error = response.json::<ApiError>().await.ok();
            match (status, error.as_ref().map(|e| e.error.as_str())) {
                (StatusCode::BAD_REQUEST, Some("authorization_pending")) => {}
                (StatusCode::BAD_REQUEST, Some("slow_down")) => {
                    pending.interval += Duration::from_secs(5)
                }
                _ => bail!("Account login failed ({status}); start a new login"),
            }
        }
    }

    /// Restore access after restart. Transient failures leave durable credentials intact.
    pub async fn resume(&self) -> anyhow::Result<()> {
        let mut state = self.inner.state.lock().await;
        if state.saved.as_ref().is_some_and(|s| s.pending_logout) {
            return self.revoke_locked(&mut state).await;
        }
        if state.saved.is_some() {
            self.refresh_locked(&mut state).await?;
        }
        Ok(())
    }

    /// Obtain an unexpired bearer token, refreshing on demand. Do not log the result.
    pub async fn access_token(&self) -> anyhow::Result<String> {
        let mut state = self.inner.state.lock().await;
        if let Some((token, deadline)) = &state.access {
            if *deadline > Instant::now() {
                return Ok(token.clone());
            }
        }
        self.refresh_locked(&mut state).await?;
        Ok(state.access.as_ref().context("Not signed in")?.0.clone())
    }

    /// Revalidate identity with the API, retrying once after access-token expiry/revocation.
    pub async fn me(&self) -> anyhow::Result<User> {
        let mut state = self.inner.state.lock().await;
        if !state
            .access
            .as_ref()
            .is_some_and(|(_, until)| *until > Instant::now())
        {
            self.refresh_locked(&mut state).await?;
        }
        for attempt in 0..2 {
            let token = &state.access.as_ref().context("Not signed in")?.0;
            let response = self
                .inner
                .client
                .get(self.endpoint("me"))
                .bearer_auth(token)
                .send()
                .await?;
            if response.status() == StatusCode::UNAUTHORIZED && attempt == 0 {
                self.refresh_locked(&mut state).await?;
                continue;
            }
            ensure!(
                response.status().is_success(),
                "Could not validate account ({})",
                response.status()
            );
            #[derive(Deserialize)]
            struct Me {
                user: User,
            }
            let user = response.json::<Me>().await?.user;
            if let Some(saved) = state.saved.as_mut() {
                saved.user = user.clone();
                self.persist(saved).await?;
            }
            self.inner
                .status
                .send_replace(Status::SignedIn(user.clone()));
            return Ok(user);
        }
        unreachable!()
    }

    /// Signs out locally before attempting revocation. Offline revocation remains as a
    /// keyring tombstone and is retried by resume/login/logout, never restored as a login.
    pub async fn logout(&self) -> anyhow::Result<()> {
        let mut state = self.inner.state.lock().await;
        state.generation += 1;
        state.access = None;
        self.inner.status.send_replace(Status::SignedOut);
        if let Some(saved) = state.saved.as_mut() {
            saved.pending_logout = true;
            if let Err(error) = self.persist(saved).await {
                // If an update fails but deletion works, still honor durable local logout.
                // If both fail, report it: we cannot promise logout across a restart.
                self.clear()
                    .await
                    .context("Could not persist logout; OS credential store is unavailable")?;
                let result = self.revoke_locked(&mut state).await;
                state.saved = None;
                return result.context(error);
            }
            self.revoke_locked(&mut state).await?;
        }
        Ok(())
    }

    async fn revoke_locked(&self, state: &mut State) -> anyhow::Result<()> {
        let Some(saved) = &state.saved else {
            return Ok(());
        };
        let response = self
            .inner
            .client
            .post(self.endpoint("logout"))
            .json(&serde_json::json!({"refresh_token": saved.refresh_token}))
            .send()
            .await
            .context("Signed out locally; server revocation will be retried")?;
        ensure!(
            response.status().is_success(),
            "Signed out locally; server revocation will be retried"
        );
        self.clear().await?;
        state.saved = None;
        Ok(())
    }

    async fn refresh_locked(&self, state: &mut State) -> anyhow::Result<()> {
        let saved = state
            .saved
            .as_ref()
            .filter(|s| !s.pending_logout)
            .context("Not signed in")?;
        let response = self
            .inner
            .client
            .post(self.endpoint("refresh"))
            .json(&serde_json::json!({"refresh_token": saved.refresh_token}))
            .send()
            .await
            .context("Could not refresh account; saved login retained")?;
        let status = response.status();
        if status.is_success() {
            self.install(
                state,
                response.json().await.context("Invalid refresh response")?,
            )
            .await?;
            return Ok(());
        }
        let error = response.json::<ApiError>().await.ok();
        if status == StatusCode::BAD_REQUEST && error.is_some_and(|e| e.error == "invalid_grant") {
            state.access = None;
            self.inner.status.send_replace(Status::SignedOut);
            // Clear durable credentials before forgetting them, so deletion errors can be retried.
            self.clear().await?;
            state.saved = None;
            bail!("Account session expired; sign in again");
        }
        bail!("Could not refresh account ({status}); saved login retained")
    }

    async fn install(&self, state: &mut State, tokens: Tokens) -> anyhow::Result<User> {
        ensure!(
            tokens.token_type.eq_ignore_ascii_case("bearer")
                && !tokens.access_token.is_empty()
                && !tokens.refresh_token.is_empty()
                && (1..=86400).contains(&tokens.expires_in),
            "Invalid account tokens"
        );
        let saved = Saved {
            refresh_token: tokens.refresh_token,
            user: tokens.user.clone(),
            pending_logout: false,
        };
        // Durable write precedes publication. Retain previous credential on write failure;
        // the API's idempotent predecessor refresh allows recovery after crash/write failure.
        self.persist(&saved).await?;
        state.saved = Some(saved);
        state.access = Some((
            tokens.access_token,
            Instant::now() + Duration::from_secs(tokens.expires_in.saturating_sub(30)),
        ));
        self.inner
            .status
            .send_replace(Status::SignedIn(tokens.user.clone()));
        Ok(tokens.user)
    }

    async fn persist(&self, saved: &Saved) -> anyhow::Result<()> {
        let value = serde_json::to_string(saved)?;
        // Keep writes inside the serialized operation even if its future is cancelled.
        // A detached spawn_blocking write could otherwise resurrect credentials after logout.
        self.inner.store.save(&value)
    }
    async fn clear(&self) -> anyhow::Result<()> {
        self.inner.store.clear()
    }
}

fn validate_origin(origin: &str) -> anyhow::Result<Url> {
    let url = Url::parse(origin).context("Invalid account API origin")?;
    let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    ensure!(
        url.scheme() == "https" || (url.scheme() == "http" && loopback),
        "Account API requires HTTPS"
    );
    ensure!(
        url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.path() == "/"
            && url.query().is_none()
            && url.fragment().is_none(),
        "Account API must be an origin without credentials, path, query or fragment"
    );
    Ok(url)
}
fn new_verifier() -> anyhow::Result<String> {
    let mut bytes = [0u8; 32];
    SysRng
        .try_fill_bytes(&mut bytes)
        .map_err(|_| anyhow::anyhow!("Could not generate login randomness"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}
fn challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

#[cfg(test)]
mod tests;
