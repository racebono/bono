//! Bono Cloud: Claude, through Bono's website, for signed-in accounts that have access to it. The
//! website speaks the Claude API at [`PATH`] (`/v1/messages`, `/v1/models`), with the account's
//! access token as the bearer token, and answers `403` to accounts without access.
//!
//! The conversation is the Claude provider's with its default settings, except for the model,
//! effort (`output_config`), `max_tokens` and the cache breakpoints' `ttl`: requests leave those
//! out, and the website chooses them. The website ignores them when a request does send them
//! (e.g. one made by hand), so they can't be changed from this side. There's nothing to set up.
//!
//! Each game session's requests draw on a [`Lease`]: a share of the account's weekly allowance
//! reserved for it up front (`/v1/sessions`). Requests without one count against a rolling
//! 24-hour ceiling instead, a fraction of the week, which a long race can run into midway.

use super::{Model, ModelOption, Reply, Tools, Usage, claude};
use crate::{
    account::{Account, Status},
    config::ClaudeConfig,
};
use anyhow::Context;
use bono_i18n::t;
use futures::future::BoxFuture;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

/// Where the API is, on the account's website
const PATH: &str = "/api/cloud";

/// How long requests go without a lease after one failed to start, before another is tried
const LEASE_RETRY: Duration = Duration::from_secs(60);

/// How long the website may take to answer anything but a conversation's request
const TIMEOUT: Duration = Duration::from_secs(30);

/// How often an engineer session's connection is pinged: often enough to keep it, and the
/// website's worker and its database connection, from going cold between turns
const KEEP_WARM: Duration = Duration::from_secs(60);

/// Turns had with Bono Cloud since the app started, answered or not
static TURNS: AtomicU64 = AtomicU64::new(0);

/// The models Bono Cloud offers `account`.
pub async fn models(
    account: Option<&Account>,
    client: &reqwest::Client,
) -> anyhow::Result<Vec<ModelOption>> {
    let account = signed_in(account)?;
    let token = access_token(account).await?;
    let result = list_models(account, &token, client).await;
    let result = if result.as_ref().is_err_and(|error| unauthorized(error)) {
        let token = refresh_token(account, &token).await?;
        list_models(account, &token, client).await
    } else {
        result
    };
    result.map_err(readable_error)
}

async fn list_models(
    account: &Account,
    token: &str,
    client: &reqwest::Client,
) -> anyhow::Result<Vec<ModelOption>> {
    claude::list_models(
        client,
        &base_url(account),
        &claude::api_headers("")?,
        Some(token),
        t!("provider.bono_cloud_unreachable"),
    )
    .await
}

/// Whether `account` may use Bono Cloud, as the website says. A website without the API (`404`)
/// doesn't offer it to anyone.
pub async fn authorized(account: &Account, client: &reqwest::Client) -> anyhow::Result<bool> {
    match models(Some(account), client).await {
        Ok(_) => Ok(true),
        Err(e)
            if e.downcast_ref::<claude::ApiError>().is_some_and(|e| {
                matches!(e.status, StatusCode::FORBIDDEN | StatusCode::NOT_FOUND)
            }) =>
        {
            Ok(false)
        }
        Err(e) => Err(e),
    }
}

/// What an account may still use of Bono Cloud, as the website says (`/v1/entitlement`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entitlement {
    /// `none` without access
    pub status: String,
    #[serde(default)]
    pub week: Option<Week>,
    /// How long the rest of the week's allowance lasts, roughly, in hours of driving
    #[serde(default)]
    pub estimated_hours_left: Option<f64>,
}

/// The week's allowance, in micro-USD.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Week {
    pub used: i64,
    pub budget: i64,
    /// Held for sessions' leases
    pub reserved: i64,
    /// When the allowance starts over (RFC 3339)
    #[serde(default)]
    pub resets_at: Option<String>,
}

/// What `account` may still use of Bono Cloud.
pub async fn entitlement(
    account: Option<&Account>,
    client: &reqwest::Client,
) -> anyhow::Result<Entitlement> {
    let account = signed_in(account)?;
    let url = format!("{}/v1/entitlement", base_url(account));
    let resp = call(account, |token| {
        client.get(&url).bearer_auth(token).timeout(TIMEOUT).send()
    })
    .await
    .map_err(readable_error)?;
    resp.json()
        .await
        .context("Bono Cloud sent an allowance that couldn't be read")
}

/// How many turns have been had with Bono Cloud: each changes what's left of the allowance.
#[inline]
pub fn turns() -> u64 {
    TURNS.load(Ordering::Relaxed)
}

/// Sends the request `send` makes with the account's token, and once more with a new one if the
/// website rejects it. A response that isn't a success is an error ([`claude::ApiError`]).
async fn call<F>(account: &Account, send: impl Fn(&str) -> F) -> anyhow::Result<reqwest::Response>
where
    F: Future<Output = reqwest::Result<reqwest::Response>>,
{
    let token = access_token(account).await?;
    let mut resp = send(&token)
        .await
        .context(t!("provider.bono_cloud_unreachable"))?;
    if resp.status() == StatusCode::UNAUTHORIZED {
        let token = refresh_token(account, &token).await?;
        resp = send(&token)
            .await
            .context(t!("provider.bono_cloud_unreachable"))?;
    }
    if !resp.status().is_success() {
        return Err(claude::ApiError::read(resp).await.into());
    }
    Ok(resp)
}

/// The game session a conversation is about, for its [`Lease`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameSession {
    /// Tells it apart from the others: its track, type and number
    pub key: String,
    /// The app's identifier for the simulator, e.g. `lmu`
    pub sim: &'static str,
    pub session_type: SessionType,
    /// How long it's expected to last: the lease reserves the allowance for as long
    pub expected_minutes: u32,
}

/// A session's type, as leases know them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionType {
    Practice,
    Qualifying,
    Race,
}

impl SessionType {
    /// The type the simulators call `session_type` (`test`, `practice`, `qualifying`, `warmup`,
    /// `race`): any but qualifying and race is practice.
    pub fn from_sim(session_type: &str) -> Self {
        match session_type {
            "qualifying" => Self::Qualifying,
            "race" => Self::Race,
            _ => Self::Practice,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Practice => "practice",
            Self::Qualifying => "qualifying",
            Self::Race => "race",
        }
    }

    /// How long a session of the type lasts, when the simulator doesn't say. Too long only
    /// reserves more: what's left is given back once the lease ends
    fn default_minutes(self) -> u32 {
        match self {
            Self::Practice => 60,
            Self::Qualifying => 30,
            Self::Race => 120,
        }
    }
}

impl GameSession {
    /// The session `key` of `sim`, of the simulator's `session_type`, scheduled to end
    /// `scheduled` seconds in, if the simulator says (timed sessions).
    pub fn new(key: String, sim: &'static str, session_type: &str, scheduled: Option<f64>) -> Self {
        /// The most a lease may be asked for, in minutes
        const MAX_MINUTES: f64 = 1440.0;

        let session_type = SessionType::from_sim(session_type);
        let expected_minutes = scheduled
            .map(|seconds| (seconds / 60.0).ceil())
            .filter(|minutes| minutes.is_finite() && *minutes > 0.0)
            .map(|minutes| minutes.clamp(1.0, MAX_MINUTES))
            .map_or(session_type.default_minutes(), |minutes| minutes as u32);
        Self {
            key,
            sim,
            session_type,
            expected_minutes,
        }
    }
}

/// A conversation's Cloud session lease: a share of the week's allowance the website reserves for
/// one game session, which its requests draw on (sent as their `session_id`) instead of the daily
/// ceiling. Started before the session's first request, and ended when the session changes, the
/// conversation starts over, or it's let go of. One never ended expires on its own, and gives
/// back what it held then.
///
/// Clones share the lease: ending any ends it.
#[derive(Clone)]
pub struct Lease(Arc<LeaseKeeper>);

struct LeaseKeeper {
    account: Account,
    client: reqwest::Client,
    base_url: String,
    /// Tells this conversation's `client_session_id`s from any other's: the website answers one
    /// it's seen before with the same lease, even once it's ended
    nonce: u64,
    state: Mutex<LeaseState>,
}

#[derive(Default)]
struct LeaseState {
    game: Option<GameSession>,
    /// The lease's id, once it's started
    id: Option<String>,
    /// How many leases were asked for, numbering the next `client_session_id`
    started: u64,
    /// After one failed to start, no other is tried before then
    retry_at: Option<Instant>,
    /// The token the last request was sent with: once the account signs out, the lease is
    /// ended with it
    token: Option<String>,
}

/// What the website answers a lease with.
#[derive(Deserialize)]
struct LeaseBody {
    id: String,
}

impl Lease {
    fn new(account: Account, client: reqwest::Client) -> Self {
        Self(Arc::new(LeaseKeeper {
            base_url: base_url(&account),
            account,
            client,
            nonce: rand::random(),
            state: Mutex::default(),
        }))
    }

    fn state(&self) -> MutexGuard<'_, LeaseState> {
        self.0.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The game session the requests are for now: another one ends the lease, and the next
    /// request starts one for it.
    fn set_game(&self, game: GameSession) {
        let ended = {
            let mut state = self.state();
            let same = state.game.as_ref().is_some_and(|g| g.key == game.key);
            state.game = Some(game);
            match same {
                true => None,
                false => {
                    state.retry_at = None;
                    state.id.take()
                }
            }
        };
        if let Some(id) = ended {
            self.end_in_background(id);
        }
    }

    /// Ends the lease, if there's one, without waiting for the website: the next request starts
    /// another.
    pub fn end(&self) {
        let ended = {
            let mut state = self.state();
            state.started += 1;
            state.retry_at = None;
            state.id.take()
        };
        if let Some(id) = ended {
            self.end_in_background(id);
        }
    }

    /// Ends the lease, if there's one, and waits for the website to say so (e.g. as the app
    /// quits, when nothing would be left to wait for it).
    pub async fn end_and_wait(&self) {
        let ended = {
            let mut state = self.state();
            state.started += 1;
            state.id.take()
        };
        if let Some(id) = ended {
            self.clone().end_lease(id).await;
        }
    }

    fn end_in_background(&self, id: String) {
        match tokio::runtime::Handle::try_current() {
            Ok(rt) => {
                rt.spawn(self.clone().end_lease(id));
            }
            Err(_) => log::debug!("Not ending Cloud session lease {id}: no runtime available"),
        }
    }

    async fn end_lease(self, id: String) {
        let keeper = &self.0;
        let url = format!("{}/v1/sessions/{id}/end", keeper.base_url);
        let send = |token: &str| {
            keeper
                .client
                .post(&url)
                .bearer_auth(token)
                .timeout(TIMEOUT)
                .send()
        };
        let result = match keeper.account.status() {
            Status::SignedIn(_) => call(&keeper.account, send).await.map(drop),
            // Signed out since: with the token it was used with, if it's still good
            Status::SignedOut => {
                let token = self.state().token.clone();
                match token {
                    Some(token) => match send(&token).await {
                        Ok(resp) if resp.status().is_success() => Ok(()),
                        Ok(resp) => Err(claude::ApiError::read(resp).await.into()),
                        Err(e) => Err(e.into()),
                    },
                    None => Err(anyhow::anyhow!("signed out")),
                }
            }
        };
        match result {
            Ok(()) => log::info!("Ended Cloud session lease {id}"),
            Err(e) => {
                log::warn!("Couldn't end Cloud session lease {id} (it expires on its own): {e:#}")
            }
        }
    }

    /// Pings the website every [`KEEP_WARM`] with a cheap authenticated request, until aborted.
    async fn keep_warm(self) {
        let keeper = &self.0;
        let url = format!("{}/v1/models", keeper.base_url);
        let mut interval = tokio::time::interval(KEEP_WARM);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        interval.tick().await;
        loop {
            interval.tick().await;
            if !matches!(keeper.account.status(), Status::SignedIn(_)) {
                continue;
            }
            let result = call(&keeper.account, |token| {
                keeper
                    .client
                    .get(&url)
                    .bearer_auth(token)
                    .headers(claude::api_headers("").unwrap_or_default())
                    .timeout(TIMEOUT)
                    .send()
            })
            .await;
            if let Err(e) = result {
                log::debug!("Couldn't keep Bono Cloud warm: {e:#}");
            }
        }
    }

    /// Asks the website for a lease for `game`.
    async fn start(&self, game: &GameSession, client_session_id: &str) -> anyhow::Result<String> {
        #[derive(Serialize)]
        struct NewLease<'a> {
            client_session_id: &'a str,
            sim: &'a str,
            session_type: &'static str,
            expected_minutes: u32,
        }

        let keeper = &self.0;
        let url = format!("{}/v1/sessions", keeper.base_url);
        let body = NewLease {
            client_session_id,
            sim: game.sim,
            session_type: game.session_type.as_str(),
            expected_minutes: game.expected_minutes,
        };
        let resp = call(&keeper.account, |token| {
            keeper
                .client
                .post(&url)
                .bearer_auth(token)
                .timeout(TIMEOUT)
                .json(&body)
                .send()
        })
        .await?;
        let lease = resp
            .json::<LeaseBody>()
            .await
            .context("Bono Cloud sent a session lease that couldn't be read")?;
        Ok(lease.id)
    }
}

/// The `client_session_id` of the `count`th lease asked for the game session `key`: at most 128
/// characters, the key (cut short if need be) last.
fn client_session_id(nonce: u64, count: u64, key: &str) -> String {
    let mut id = format!("{nonce:016x}-{count}-{key}");
    if let Some((end, _)) = id.char_indices().nth(128) {
        id.truncate(end);
    }
    id
}

impl claude::BearerSource for Lease {
    fn token<'a>(&'a self, rejected: Option<&'a str>) -> BoxFuture<'a, anyhow::Result<String>> {
        Box::pin(async move {
            let account = &self.0.account;
            let token = match rejected {
                Some(token) => refresh_token(account, token).await,
                None => access_token(account).await,
            }?;
            self.state().token = Some(token.clone());
            Ok(token)
        })
    }
}

impl claude::LeaseSource for Lease {
    fn lease<'a>(
        &'a self,
        rejected: Option<&'a str>,
    ) -> BoxFuture<'a, anyhow::Result<Option<String>>> {
        Box::pin(async move {
            let (game, client_session_id, generation) = {
                let mut state = self.state();
                if rejected.is_some() && state.id.as_deref() == rejected {
                    state.id = None;
                }
                let Some(game) = state.game.clone() else {
                    return Ok(None);
                };
                if let Some(id) = &state.id {
                    return Ok(Some(id.clone()));
                }
                if state.retry_at.is_some_and(|at| Instant::now() < at) {
                    return Ok(None);
                }
                state.started += 1;
                let id = client_session_id(self.0.nonce, state.started, &game.key);
                (game, id, state.started)
            };

            match self.start(&game, &client_session_id).await {
                Ok(id) => {
                    let mut state = self.state();
                    let current = state.game.as_ref().is_some_and(|g| g.key == game.key);
                    if current && state.started == generation && state.id.is_none() {
                        log::info!(
                            "Started Cloud session lease {id} for {} ({} minutes)",
                            game.key,
                            game.expected_minutes
                        );
                        state.id = Some(id.clone());
                        return Ok(Some(id));
                    }
                    // The session changed meanwhile: the lease isn't this one's
                    drop(state);
                    self.end_in_background(id);
                    Ok(None)
                }
                Err(e) if no_lease_allowed(&e) => Err(readable_error(e)),
                Err(e) => {
                    log::warn!(
                        "Couldn't start a Cloud session lease, asking without one (it counts \
                         against the daily ceiling): {e:#}"
                    );
                    self.state().retry_at = Some(Instant::now() + LEASE_RETRY);
                    Ok(None)
                }
            }
        })
    }
}

/// Whether the website won't lease anything to the account: nothing's left this week, or it has
/// no access.
fn no_lease_allowed(error: &anyhow::Error) -> bool {
    error.downcast_ref::<claude::ApiError>().is_some_and(|e| {
        e.status == StatusCode::FORBIDDEN || e.code.as_deref() == Some("allowance_exhausted")
    })
}

/// A conversation with Bono Cloud, as `account`.
pub struct Conversation {
    inner: claude::Conversation,
    lease: Lease,
    /// Keeps the website warm while the engineer follows a game session
    keep_warm: Option<tokio::task::AbortHandle>,
}

impl Conversation {
    pub fn new(
        account: Option<&Account>,
        system: Arc<str>,
        tools: Tools,
        client: reqwest::Client,
    ) -> anyhow::Result<Self> {
        let account = signed_in(account)?;
        let lease = Lease::new(account.clone(), client.clone());
        // The rest is Claude's defaults: none of it is the pilot's to change
        let mut builder = claude::ConversationBuilder::from_config(&ClaudeConfig::default());
        builder
            .history_limits(claude::HistoryLimits {
                max_bytes: 120_000,
                max_messages: 100,
                target: 0.5,
            })
            .base_url(base_url(account))
            .bearer_source(Arc::new(lease.clone()))
            .lease_source(Arc::new(lease.clone()))
            .server_settings()
            .system(system)
            .tools(tools);
        Ok(Self {
            inner: builder.build(client)?,
            lease,
            keep_warm: None,
        })
    }
}

impl Model for Conversation {
    fn send_streaming<'a>(
        &'a mut self,
        text: String,
        on_text: &'a mut (dyn FnMut(&str) + Send),
    ) -> BoxFuture<'a, anyhow::Result<Reply>> {
        Box::pin(async move {
            let reply = self.inner.send_streaming(text, on_text).await;
            TURNS.fetch_add(1, Ordering::Relaxed);
            reply.map_err(readable_error)
        })
    }

    /// Also ends the lease: the next request starts another.
    fn reset(&mut self) {
        self.inner.reset();
        self.lease.end();
    }

    #[inline]
    fn usage(&self) -> &BTreeMap<String, Usage> {
        self.inner.usage()
    }

    fn set_game_session(&mut self, game: &GameSession) {
        self.lease.set_game(game.clone());
        if self.keep_warm.is_none()
            && let Ok(rt) = tokio::runtime::Handle::try_current()
        {
            self.keep_warm = Some(rt.spawn(self.lease.clone().keep_warm()).abort_handle());
        }
    }

    fn lease(&self) -> Option<Lease> {
        Some(self.lease.clone())
    }
}

impl Drop for Conversation {
    fn drop(&mut self) {
        if let Some(keep_warm) = self.keep_warm.take() {
            keep_warm.abort();
        }
        self.lease.end();
    }
}

fn signed_in(account: Option<&Account>) -> anyhow::Result<&Account> {
    account
        .filter(|account| matches!(account.status(), Status::SignedIn(_)))
        .with_context(|| t!("provider.bono_cloud_sign_in"))
}

async fn access_token(account: &Account) -> anyhow::Result<String> {
    account
        .access_token()
        .await
        .with_context(|| token_error(account))
}

async fn refresh_token(account: &Account, rejected: &str) -> anyhow::Result<String> {
    account
        .refresh_access_token(rejected)
        .await
        .with_context(|| token_error(account))
}

fn token_error(account: &Account) -> &'static str {
    match account.status() {
        Status::SignedOut => t!("provider.bono_cloud_session_ended"),
        Status::SignedIn(_) => t!("provider.bono_cloud_refresh_failed"),
    }
}

fn unauthorized(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<claude::ApiError>()
        .is_some_and(|error| error.status == StatusCode::UNAUTHORIZED)
}

fn readable_error(error: anyhow::Error) -> anyhow::Error {
    let Some(api) = error.downcast_ref::<claude::ApiError>() else {
        return error;
    };
    let readable = match (api.status, api.code.as_deref()) {
        (StatusCode::UNAUTHORIZED, _) => t!("provider.bono_cloud_session_ended"),
        (StatusCode::FORBIDDEN, _) => t!("provider.bono_cloud_unavailable"),
        (_, Some("allowance_exhausted")) => t!("provider.bono_cloud_no_allowance"),
        (_, Some("daily_allowance_exhausted")) => t!("provider.bono_cloud_daily_allowance"),
        (StatusCode::PAYLOAD_TOO_LARGE, _) => t!("provider.bono_cloud_too_long"),
        _ => return error,
    };
    error.context(readable)
}

fn base_url(account: &Account) -> String {
    account.url(PATH).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        account::tests::{expire_access_token, signed_in, tokens},
        provider::{StopReason, ToolHandler},
    };
    use reqwest::Url;
    use serde_json::{Value, json};
    use std::sync::Mutex;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    /// Answers each request with the next of `responses` (status and body: a stream of events
    /// when it starts with `event:`, JSON otherwise), and records each request's head
    /// (lowercased) and body. A status of 0 answers nothing.
    async fn mock(responses: Vec<(u16, String)>) -> (Url, Arc<Mutex<Vec<(String, Value)>>>) {
        mock_headers(responses, "").await
    }

    async fn mock_headers(
        responses: Vec<(u16, String)>,
        headers: &'static str,
    ) -> (Url, Arc<Mutex<Vec<(String, Value)>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();

        tokio::spawn(async move {
            for (status, body) in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buf = Vec::new();
                let mut chunk = [0; 4096];
                let body_start = loop {
                    let n = stream.read(&mut chunk).await.unwrap();
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break end + 4;
                    }
                };
                let head = String::from_utf8_lossy(&buf[..body_start]).to_lowercase();
                let length = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .map_or(0, |n| n.trim().parse::<usize>().unwrap());
                while buf.len() < body_start + length {
                    let n = stream.read(&mut chunk).await.unwrap();
                    buf.extend_from_slice(&chunk[..n]);
                }
                let request = serde_json::from_slice(&buf[body_start..]).unwrap_or(Value::Null);
                recorded.lock().unwrap().push((head, request));

                if status == 0 {
                    continue;
                }

                let content_type = match body.starts_with("event:") {
                    true => "text/event-stream",
                    false => "application/json",
                };
                let response = format!(
                    "HTTP/1.1 {status} Mock\r\n{headers}content-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });

        (url, requests)
    }

    fn model_list() -> (u16, String) {
        (
            200,
            json!({"data": [{"id": "claude-sonnet-5-5"}], "has_more": false}).to_string(),
        )
    }

    #[tokio::test]
    async fn asks_the_website_whether_the_account_may_use_it() {
        let (url, requests) = mock(vec![
            model_list(),
            (403, json!({"type": "error", "error": {"type": "permission_error", "message": "forbidden"}}).to_string()),
        ])
        .await;
        let account = signed_in(url, "access-1");
        let client = reqwest::Client::new();

        assert!(authorized(&account, &client).await.unwrap());
        assert!(!authorized(&account, &client).await.unwrap());

        let requests = requests.lock().unwrap();
        assert!(requests[0].0.starts_with("get /api/cloud/v1/models?"));
        assert!(requests[0].0.contains("authorization: bearer access-1"));
        assert!(requests[0].0.contains("anthropic-version: 2023-06-01"));
    }

    #[tokio::test]
    async fn website_without_the_api_offers_nothing() {
        let (url, _) = mock(vec![(404, "<!doctype html><title>404</title>".into())]).await;
        let account = signed_in(url, "access-1");
        assert!(!authorized(&account, &reqwest::Client::new()).await.unwrap());
    }

    #[tokio::test]
    async fn talks_as_the_account_leaving_the_settings_to_the_website() {
        let (url, requests) = mock(vec![answer()]).await;
        let account = signed_in(url, "access-1");
        let mut conversation = conversation(&account, Tools::default());

        let reply = conversation.send("Pit?".into()).await.unwrap();
        assert_eq!(reply.text, "Box.");
        assert_eq!(reply.stop, StopReason::EndTurn);
        assert_eq!(reply.model, "claude-sonnet-5-5");

        let requests = requests.lock().unwrap();
        let (head, body) = &requests[0];
        assert!(head.starts_with("post /api/cloud/v1/messages "));
        assert!(head.contains("authorization: bearer access-1"));
        assert!(!head.contains("x-api-key"));
        assert!(!head.contains("oauth-2025-04-20"));

        // The website chooses the model and its settings
        assert!(body.get("model").is_none());
        assert!(body.get("output_config").is_none());
        assert!(body.get("max_tokens").is_none());
        assert_eq!(body["cache_control"], json!({"type": "ephemeral"}));
        assert_eq!(
            body["system"][0]["cache_control"],
            json!({"type": "ephemeral"})
        );
        // The rest is what the Claude provider sends with its defaults
        assert_eq!(body["stream"], true);
        assert_eq!(body["fallbacks"], "default");
        assert!(body.get("context_management").is_none());
        assert_eq!(body["system"][0]["text"], "You are a race engineer.");
        assert_eq!(
            body["messages"],
            json!([{"role": "user", "content": "Pit?"}])
        );
    }

    fn conversation(account: &Account, tools: Tools) -> Conversation {
        Conversation::new(
            Some(account),
            "You are a race engineer.".into(),
            tools,
            reqwest::Client::new(),
        )
        .unwrap()
    }

    fn answer() -> (u16, String) {
        let message = json!({
            "model": "claude-sonnet-5-5",
            "stop_reason": "end_turn",
            "content": [{"type": "text", "text": "Box."}],
        });
        (200, message.to_string())
    }

    #[tokio::test]
    async fn long_sessions_keep_answering_with_bounded_history() {
        let (url, requests) = mock((0..120).map(|_| answer()).collect()).await;
        let account = signed_in(url, "access-1");
        let mut conversation = conversation(&account, Tools::default());
        for index in 0..120 {
            let question = format!(
                "Question {index} [BEGIN UNTRUSTED TELEMETRY SNAPSHOT]{}[END UNTRUSTED TELEMETRY SNAPSHOT]",
                "x".repeat(2000)
            );
            assert_eq!(conversation.send(question).await.unwrap().text, "Box.");
        }
        let requests = requests.lock().unwrap();
        let mut trimmed = false;
        for pair in requests.windows(2) {
            trimmed |= pair[1].1["messages"].as_array().unwrap().len()
                < pair[0].1["messages"].as_array().unwrap().len();
        }
        assert!(trimmed);
        for (_, body) in requests.iter() {
            assert!(serde_json::to_vec(body).unwrap().len() <= 120_000);
            let messages = body["messages"].as_array().unwrap();
            assert!(messages.len() <= 100);
            assert_eq!(messages[0]["role"], "user");
        }
    }

    fn rejected() -> (u16, String) {
        let error = json!({"type": "error", "error": {"type": "authentication_error", "message": "invalid token"}});
        (401, error.to_string())
    }

    fn lease_body(id: &str) -> (u16, String) {
        (
            200,
            json!({"id": id, "status": "active", "expires_at": "2099-01-01T00:00:00Z"}).to_string(),
        )
    }

    fn cloud_error(status: u16, code: &str) -> (u16, String) {
        (
            status,
            json!({"type": "error", "error": {"type": "api_error", "code": code, "message": code}})
                .to_string(),
        )
    }

    fn game(key: &str) -> GameSession {
        GameSession::new(key.into(), "lmu", "race", Some(5400.0))
    }

    #[tokio::test]
    async fn leases_once_per_session_and_ends_before_the_next() {
        let (url, requests) = mock(vec![
            lease_body("first"),
            answer(),
            answer(),
            (200, json!({"ok": true}).to_string()),
            lease_body("second"),
            answer(),
        ])
        .await;
        let account = signed_in(url, "access-1");
        let mut conversation = conversation(&account, Tools::default());
        conversation.set_game_session(&game("track|race|1"));
        conversation.send("Pit?".into()).await.unwrap();
        conversation.send("Now?".into()).await.unwrap();
        conversation.set_game_session(&game("track|race|2"));
        tokio::time::timeout(Duration::from_secs(2), async {
            while requests.lock().unwrap().len() < 4 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        conversation.send("Pit?".into()).await.unwrap();
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 6);
        assert!(requests[0].0.starts_with("post /api/cloud/v1/sessions "));
        assert_eq!(requests[0].1["sim"], "lmu");
        assert_eq!(requests[0].1["session_type"], "race");
        assert_eq!(requests[0].1["expected_minutes"], 90);
        assert_eq!(requests[1].1["session_id"], "first");
        assert_eq!(requests[2].1["session_id"], "first");
        assert!(
            requests[3]
                .0
                .starts_with("post /api/cloud/v1/sessions/first/end ")
        );
        assert_ne!(
            requests[0].1["client_session_id"],
            requests[4].1["client_session_id"]
        );
        assert_eq!(requests[5].1["session_id"], "second");
    }

    #[tokio::test]
    async fn replaces_invalid_lease_once() {
        for fails_again in [false, true] {
            let last = if fails_again {
                cloud_error(409, "invalid_session")
            } else {
                answer()
            };
            let (url, requests) = mock(vec![
                lease_body("old"),
                cloud_error(409, "invalid_session"),
                lease_body("new"),
                last,
            ])
            .await;
            let account = signed_in(url, "access-1");
            let mut conversation = conversation(&account, Tools::default());
            conversation.set_game_session(&game("track|race|1"));
            assert_eq!(conversation.send("Pit?".into()).await.is_err(), fails_again);
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 4);
            assert_ne!(
                requests[0].1["client_session_id"],
                requests[2].1["client_session_id"]
            );
            assert_eq!(requests[1].1["session_id"], "old");
            assert_eq!(requests[3].1["session_id"], "new");
            assert_eq!(requests[1].1["messages"], requests[3].1["messages"]);
        }
    }

    #[tokio::test]
    async fn lease_failure_falls_back_but_denied_allowance_stops_the_turn() {
        for (status, code, denied) in [
            (503, "unavailable", false),
            (429, "allowance_exhausted", true),
            (403, "forbidden", true),
        ] {
            let mut responses = vec![cloud_error(status, code)];
            if !denied {
                responses.extend([answer(), answer()]);
            }
            let (url, requests) = mock(responses).await;
            let account = signed_in(url, "access-1");
            let mut conversation = conversation(&account, Tools::default());
            conversation.set_game_session(&game("track|race|1"));
            assert_eq!(conversation.send("Pit?".into()).await.is_err(), denied);
            if !denied {
                conversation.send("Now?".into()).await.unwrap();
                let requests = requests.lock().unwrap();
                assert_eq!(requests.len(), 3);
                assert!(requests[1].1.get("session_id").is_none());
                assert!(requests[2].1.get("session_id").is_none());
            } else {
                assert_eq!(requests.lock().unwrap().len(), 1);
            }
        }
    }

    #[tokio::test]
    async fn long_retry_after_never_holds_the_turn() {
        for headers in [
            "retry-after: 86400\r\nx-should-retry: false\r\n",
            "retry-after: 86400\r\nx-should-retry: true\r\n",
        ] {
            let (url, requests) =
                mock_headers(vec![cloud_error(429, "rate_limited")], headers).await;
            let account = signed_in(url, "access-1");
            let mut conversation = conversation(&account, Tools::default());
            let result =
                tokio::time::timeout(Duration::from_secs(1), conversation.send("Pit?".into()))
                    .await
                    .unwrap();
            assert!(result.is_err());
            assert_eq!(requests.lock().unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn refreshes_rejected_chat_and_retries_the_same_request() {
        let (url, requests) = mock(vec![rejected(), (200, tokens().to_string()), answer()]).await;
        let account = signed_in(url, "access-old");
        let mut conversation = conversation(&account, Tools::default());
        let mut pieces = Vec::new();
        let reply = conversation
            .send_streaming("Pit?".into(), &mut |piece| pieces.push(piece.to_owned()))
            .await
            .unwrap();
        assert_eq!(reply.text, "Box.");
        assert_eq!(pieces, ["Box."]);
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[0].0.contains("authorization: bearer access-old"));
        assert!(requests[1].0.starts_with("post /api/desktop/auth/refresh "));
        assert_eq!(requests[1].1["refresh_token"], "refresh-old");
        assert!(requests[2].0.contains("authorization: bearer access-new"));
        assert_eq!(requests[0].1, requests[2].1);
        assert_eq!(requests[2].1["messages"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn revoked_chat_signs_out_with_a_readable_error() {
        let (url, requests) = mock(vec![
            rejected(),
            (400, json!({"error": "invalid_grant"}).to_string()),
        ])
        .await;
        let account = signed_in(url, "access-old");
        let mut conversation = conversation(&account, Tools::default());
        let mut status = account.subscribe();
        let error = conversation.send("Pit?".into()).await.unwrap_err();
        assert_eq!(error.to_string(), t!("provider.bono_cloud_session_ended"));
        assert_eq!(account.status(), Status::SignedOut);
        assert_eq!(*status.borrow_and_update(), Status::SignedOut);
        assert!(account.access_token().await.is_err());
        assert_eq!(requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn refreshes_rejected_models_before_checking_access() {
        let (url, requests) =
            mock(vec![rejected(), (200, tokens().to_string()), model_list()]).await;
        let account = signed_in(url, "access-old");
        assert!(authorized(&account, &reqwest::Client::new()).await.unwrap());
        let requests = requests.lock().unwrap();
        assert!(requests[0].0.starts_with("get /api/cloud/v1/models?"));
        assert!(requests[2].0.starts_with("get /api/cloud/v1/models?"));
        assert!(requests[2].0.contains("authorization: bearer access-new"));
    }

    #[tokio::test]
    async fn refresh_network_failure_retains_the_session() {
        let (url, requests) = mock(vec![rejected(), (0, String::new())]).await;
        let account = signed_in(url, "access-old");
        let mut conversation = conversation(&account, Tools::default());
        let error = conversation.send("Pit?".into()).await.unwrap_err();
        assert_eq!(error.to_string(), t!("provider.bono_cloud_refresh_failed"));
        assert!(matches!(account.status(), Status::SignedIn(_)));
        assert_eq!(account.access_token().await.unwrap(), "access-old");
        assert_eq!(requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn retries_unauthorized_only_once_and_explains_forbidden() {
        let (url, requests) = mock(vec![
            rejected(),
            (200, tokens().to_string()),
            rejected(),
            (403, json!({"type": "error", "error": {"type": "permission_error", "message": "forbidden"}}).to_string()),
            answer(),
        ])
        .await;
        let account = signed_in(url, "access-old");
        let mut conversation = conversation(&account, Tools::default());
        let error = conversation.send("Pit?".into()).await.unwrap_err();
        assert_eq!(error.to_string(), t!("provider.bono_cloud_session_ended"));
        let error = conversation.send("Pit?".into()).await.unwrap_err();
        assert_eq!(error.to_string(), t!("provider.bono_cloud_unavailable"));
        assert_eq!(conversation.send("Pit?".into()).await.unwrap().text, "Box.");
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 5);
        assert_eq!(requests[0].1, requests[4].1);
    }

    struct ExpireBearer {
        account: Account,
        expire: bool,
    }

    impl ToolHandler for ExpireBearer {
        type Input = Value;

        fn handle(
            &self,
            _: Value,
        ) -> impl Future<Output = anyhow::Result<String>> + Send + 'static {
            let account = self.account.clone();
            let expire = self.expire;
            async move {
                if expire {
                    expire_access_token(&account).await;
                }
                Ok("Gap: 2s".into())
            }
        }
    }

    #[tokio::test]
    async fn renews_bearer_between_tool_requests_without_replaying_the_turn() {
        for expire in [true, false] {
            let thinking =
                json!({"type": "thinking", "thinking": "Gap first.", "signature": "signed"});
            let call = json!({
                "model": "claude-sonnet-5-5",
                "stop_reason": "tool_use",
                "content": [
                    thinking,
                    {"type": "text", "text": "Checking."},
                    {"type": "tool_use", "id": "gap-1", "name": "gap", "input": {}},
                ],
            });
            let mut responses = vec![lease_body("tool-session"), (200, call.to_string())];
            if !expire {
                responses.push(rejected());
            }
            responses.extend([(200, tokens().to_string()), answer()]);
            let (url, requests) = mock(responses).await;
            let account = signed_in(url, "access-old");
            let mut tools = Tools::default();
            tools.insert(
                "gap",
                None,
                json!({"type": "object"}),
                ExpireBearer {
                    account: account.clone(),
                    expire,
                },
            );
            let mut conversation = conversation(&account, tools);
            conversation.set_game_session(&game("track|race|1"));
            let reply = conversation.send("Pit?".into()).await.unwrap();
            assert_eq!(reply.text, "Checking.\n\nBox.");
            let requests = requests.lock().unwrap();
            let last = requests.last().unwrap();
            for (_, body) in requests
                .iter()
                .filter(|(head, _)| head.starts_with("post /api/cloud/v1/messages "))
            {
                assert_eq!(body["session_id"], "tool-session");
            }
            assert!(last.0.contains("authorization: bearer access-new"));
            let messages = last.1["messages"].as_array().unwrap();
            assert_eq!(messages.len(), 3);
            // The signed thinking goes back as received
            assert_eq!(messages[1]["content"][0], thinking);
            assert_eq!(messages[1]["content"][2]["id"], "gap-1");
            assert_eq!(messages[2]["content"][0]["tool_use_id"], "gap-1");
            assert_eq!(messages[2]["content"][0]["content"], "Gap: 2s");
            if !expire {
                assert_eq!(requests[2].1, last.1);
            }
        }
    }

    #[tokio::test]
    async fn never_retries_a_failed_stream_after_emitting_text() {
        let events = [
            json!({"type": "message_start", "message": {"model": "claude-sonnet-5-5", "content": []}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Box."}}),
        ]
        .iter()
        .map(|event| format!("event: {}\ndata: {event}\n\n", event["type"].as_str().unwrap()))
        .collect::<String>();
        let (url, requests) = mock(vec![(200, events)]).await;
        let account = signed_in(url, "access-old");
        let mut conversation = conversation(&account, Tools::default());
        let mut text = String::new();
        assert!(
            conversation
                .send_streaming("Pit?".into(), &mut |piece| text.push_str(piece))
                .await
                .is_err()
        );
        assert_eq!(text, "Box.");
        assert_eq!(requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn needs_a_signed_in_account() {
        let error = Conversation::new(
            None,
            Arc::default(),
            Tools::default(),
            reqwest::Client::new(),
        )
        .err()
        .unwrap();
        assert_eq!(error.to_string(), t!("provider.bono_cloud_sign_in"));
    }
}
