use super::*;
use std::sync::{
    Mutex as StdMutex,
    atomic::{AtomicBool, Ordering},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Default)]
struct MemoryStore {
    value: StdMutex<Option<String>>,
    fail_write: AtomicBool,
}
impl CredentialStore for MemoryStore {
    fn load(&self) -> anyhow::Result<Option<String>> {
        Ok(self.value.lock().unwrap().clone())
    }
    fn save(&self, value: &str) -> anyhow::Result<()> {
        ensure!(!self.fail_write.load(Ordering::SeqCst), "Store unavailable");
        *self.value.lock().unwrap() = Some(value.into());
        Ok(())
    }
    fn clear(&self) -> anyhow::Result<()> {
        *self.value.lock().unwrap() = None;
        Ok(())
    }
}
fn user() -> User {
    User {
        id: "user-1".into(),
        email: "driver@example.com".into(),
        name: "Driver".into(),
        image: None,
    }
}
fn saved(store: &MemoryStore) -> Saved {
    serde_json::from_str(&store.load().unwrap().unwrap()).unwrap()
}
fn seed(store: &MemoryStore) {
    store
        .save(
            &serde_json::to_string(&Saved {
                refresh_token: "refresh-old".into(),
                user: user(),
                pending_logout: false,
            })
            .unwrap(),
        )
        .unwrap();
}
/// Signed in as [`user`] against `base`, with `access_token` good for an hour, for other
/// modules' tests of APIs that take it.
pub(crate) fn signed_in(base: Url, access_token: &str) -> Account {
    let store = MemoryStore::default();
    seed(&store);
    let saved = saved(&store);
    let (status, _) = watch::channel(Status::SignedIn(saved.user.clone()));
    Account {
        inner: Arc::new(Inner {
            base,
            client: Client::new(),
            store: Arc::new(store),
            state: Mutex::new(State {
                saved: Some(saved),
                access: Some((
                    access_token.into(),
                    Instant::now() + Duration::from_secs(3600),
                )),
                generation: 0,
            }),
            status,
        }),
    }
}
fn tokens() -> serde_json::Value {
    serde_json::json!({"token_type":"Bearer","access_token":"access-new","refresh_token":"refresh-new","expires_in":3600,"user":user()})
}

/// Minimal HTTP fixture checks real request paths/bodies and bearer headers without
/// invoking the system browser or live production credentials.
async fn server(
    replies: Vec<(&'static str, u16, serde_json::Value, &'static str)>,
) -> (Url, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let origin = base.origin().ascii_serialization();
    let handle = tokio::spawn(async move {
        for (path, status, body, expected) in replies {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let end;
            loop {
                let mut part = [0; 4096];
                let n = stream.read(&mut part).await.unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&part[..n]);
                if let Some(pos) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    end = pos + 4;
                    break;
                }
            }
            let headers = String::from_utf8_lossy(&bytes[..end]).to_lowercase();
            let length = headers
                .lines()
                .find_map(|l| l.strip_prefix("content-length: "))
                .map(|n| n.parse::<usize>().unwrap())
                .unwrap_or(0);
            while bytes.len() < end + length {
                let mut part = [0; 4096];
                let n = stream.read(&mut part).await.unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&part[..n]);
            }
            let request = String::from_utf8(bytes).unwrap();
            assert!(
                request.lines().next().unwrap().contains(path),
                "wrong request path"
            );
            assert!(
                request.to_lowercase().contains(expected),
                "missing expected request field"
            );
            let body = body.to_string().replace("$ORIGIN", &origin);
            stream.write_all(format!("HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        }
    });
    (base, handle)
}

#[test]
fn pkce_matches_rfc7636_vector_and_origins_are_restricted() {
    assert_eq!(
        challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    );
    assert_eq!(new_verifier().unwrap().len(), 43);
    for origin in [
        "http://example.com",
        "https://user:secret@example.com",
        "https://example.com/path",
        "https://example.com?x=y",
    ] {
        assert!(validate_origin(origin).is_err());
    }
    assert!(validate_origin("https://example.com").is_ok());
    assert!(validate_origin("http://127.0.0.1:4321").is_ok());
}

#[tokio::test]
async fn restart_refreshes_saved_credential_and_logout_survives_another_restart() {
    let store = Arc::new(MemoryStore::default());
    seed(&store);
    let (base, http) = server(vec![
        ("/refresh", 200, tokens(), "refresh-old"),
        (
            "/me",
            200,
            serde_json::json!({"user":user()}),
            "authorization: bearer access-new",
        ),
        (
            "/logout",
            200,
            serde_json::json!({"ok":true}),
            "refresh-new",
        ),
    ])
    .await;
    let account = Account::with_store(base.clone(), store.clone())
        .await
        .unwrap();
    assert_eq!(account.status(), Status::SignedIn(user()));
    account.resume().await.unwrap();
    assert_eq!(saved(&store).refresh_token, "refresh-new");
    assert_eq!(account.me().await.unwrap(), user());
    account.logout().await.unwrap();
    assert_eq!(account.status(), Status::SignedOut);
    drop(account);
    let reopened = Account::with_store(base, store.clone()).await.unwrap();
    assert_eq!(reopened.status(), Status::SignedOut);
    assert!(store.load().unwrap().is_none());
    http.await.unwrap();
}

#[tokio::test]
async fn transient_failure_retains_login_but_invalid_grant_removes_it() {
    let store = Arc::new(MemoryStore::default());
    seed(&store);
    let (base, http) = server(vec![
        (
            "/refresh",
            503,
            serde_json::json!({"error":"temporarily_unavailable"}),
            "refresh-old",
        ),
        (
            "/refresh",
            400,
            serde_json::json!({"error":"invalid_grant"}),
            "refresh-old",
        ),
    ])
    .await;
    let account = Account::with_store(base, store.clone()).await.unwrap();
    assert!(account.resume().await.is_err());
    assert_eq!(saved(&store).refresh_token, "refresh-old");
    assert_eq!(account.status(), Status::SignedIn(user()));
    assert!(account.resume().await.is_err());
    assert!(store.load().unwrap().is_none());
    assert_eq!(account.status(), Status::SignedOut);
    http.await.unwrap();
}

#[tokio::test]
async fn failed_rotation_write_keeps_previous_durable_token_for_retry() {
    let store = Arc::new(MemoryStore::default());
    seed(&store);
    let (base, http) = server(vec![
        ("/refresh", 200, tokens(), "refresh-old"),
        ("/refresh", 200, tokens(), "refresh-old"),
    ])
    .await;
    let account = Account::with_store(base, store.clone()).await.unwrap();
    store.fail_write.store(true, Ordering::SeqCst);
    assert!(account.resume().await.is_err());
    assert_eq!(saved(&store).refresh_token, "refresh-old");
    store.fail_write.store(false, Ordering::SeqCst);
    account.resume().await.unwrap();
    assert_eq!(saved(&store).refresh_token, "refresh-new");
    http.await.unwrap();
}

#[tokio::test]
async fn offline_logout_cannot_restore_login_and_retries_revocation() {
    let store = Arc::new(MemoryStore::default());
    seed(&store);
    let (base, http) = server(vec![
        (
            "/logout",
            503,
            serde_json::json!({"error":"temporarily_unavailable"}),
            "refresh-old",
        ),
        (
            "/logout",
            200,
            serde_json::json!({"ok":true}),
            "refresh-old",
        ),
    ])
    .await;
    let account = Account::with_store(base.clone(), store.clone())
        .await
        .unwrap();
    assert!(account.logout().await.is_err());
    assert!(saved(&store).pending_logout);
    assert!(account.access_token().await.is_err());
    drop(account);
    let account = Account::with_store(base, store.clone()).await.unwrap();
    assert_eq!(account.status(), Status::SignedOut);
    account.resume().await.unwrap();
    assert!(store.load().unwrap().is_none());
    http.await.unwrap();
}

#[tokio::test]
async fn cancelled_browser_login_cannot_resurrect_a_logged_out_session() {
    let store = Arc::new(MemoryStore::default());
    let base = validate_origin("https://example.com").unwrap();
    let account = Account::with_store(base.clone(), store).await.unwrap();
    let pending = PendingLogin {
        verification_uri: base,
        device_code: "device".into(),
        verifier: "verifier".into(),
        deadline: Instant::now() + Duration::from_secs(60),
        interval: Duration::ZERO,
        generation: 0,
        owner: account.inner.clone(),
    };
    account.logout().await.unwrap();
    assert!(
        account
            .complete_login(pending)
            .await
            .unwrap_err()
            .to_string()
            .contains("cancelled")
    );
}

#[tokio::test]
async fn browser_approval_polls_then_persists_login_before_publishing_identity() {
    let store = Arc::new(MemoryStore::default());
    let (base, http) = server(vec![
        ("/start", 200, serde_json::json!({"device_code":"device-secret","verification_uri":"$ORIGIN/desktop/authorize?request=public-id","expires_in":600,"interval":1}), "\"code_challenge_method\":\"s256\""),
        ("/token", 400, serde_json::json!({"error":"authorization_pending"}), "\"code_verifier\":"),
        ("/token", 200, tokens(), "device-secret"),
    ]).await;
    let account = Account::with_store(base.clone(), store.clone())
        .await
        .unwrap();
    let pending = account.begin_login().await.unwrap();
    assert_eq!(pending.verification_uri.origin(), base.origin());
    assert!(!pending.verification_uri.as_str().contains("device-secret"));
    assert_eq!(account.status(), Status::SignedOut);
    assert_eq!(account.complete_login(pending).await.unwrap(), user());
    assert_eq!(saved(&store).refresh_token, "refresh-new");
    assert_eq!(account.status(), Status::SignedIn(user()));
    http.await.unwrap();
}

#[tokio::test]
async fn me_refreshes_once_on_unauthorized_and_concurrent_access_uses_one_refresh() {
    let store = Arc::new(MemoryStore::default());
    seed(&store);
    let (base, http) = server(vec![
        ("/refresh", 200, tokens(), "refresh-old"),
        (
            "/me",
            401,
            serde_json::json!({"error":"invalid_token"}),
            "authorization: bearer access-new",
        ),
        ("/refresh", 200, tokens(), "refresh-new"),
        (
            "/me",
            200,
            serde_json::json!({"user":user()}),
            "authorization: bearer access-new",
        ),
    ])
    .await;
    let account = Account::with_store(base, store).await.unwrap();
    let (a, b) = tokio::join!(account.access_token(), account.access_token());
    assert_eq!(a.unwrap(), b.unwrap());
    assert_eq!(account.me().await.unwrap(), user());
    http.await.unwrap();
}

#[tokio::test]
async fn login_rejects_cross_origin_verification_url() {
    let (base, http) = server(vec![("/start", 200, serde_json::json!({"device_code":"secret","verification_uri":"https://evil.example/authorize","expires_in":600,"interval":5}), "s256")]).await;
    let account = Account::with_store(base, Arc::new(MemoryStore::default()))
        .await
        .unwrap();
    assert!(
        account
            .begin_login()
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("Untrusted")
    );
    http.await.unwrap();
}
