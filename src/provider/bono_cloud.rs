//! Bono Cloud: Claude, through Bono's website, for signed-in accounts that have access to it. The
//! website speaks the Claude API at [`PATH`] (`/v1/messages`, `/v1/models`), with the account's
//! access token as the bearer token, and answers `403` to accounts without access.
//!
//! The conversation is the Claude provider's with its default settings, except for the model,
//! effort (`output_config`), `max_tokens` and the cache breakpoints' `ttl`: requests leave those
//! out, and the website chooses them. The website ignores them when a request does send them
//! (e.g. one made by hand), so they can't be changed from this side. There's nothing to set up.

use super::{Model, ModelOption, Reply, Tools, Usage, claude};
use crate::{
    account::{Account, Status},
    config::ClaudeConfig,
};
use anyhow::Context;
use bono_i18n::t;
use futures::future::BoxFuture;
use reqwest::StatusCode;
use std::{collections::BTreeMap, sync::Arc};

/// Where the API is, on the account's website
const PATH: &str = "/api/cloud";

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

/// A conversation with Bono Cloud, as `account`.
pub struct Conversation {
    inner: claude::Conversation,
}

struct Bearer(Account);

impl claude::BearerSource for Bearer {
    fn token<'a>(&'a self, rejected: Option<&'a str>) -> BoxFuture<'a, anyhow::Result<String>> {
        Box::pin(async move {
            match rejected {
                Some(token) => refresh_token(&self.0, token).await,
                None => access_token(&self.0).await,
            }
        })
    }
}

impl Conversation {
    pub fn new(
        account: Option<&Account>,
        system: Arc<str>,
        tools: Tools,
        client: reqwest::Client,
    ) -> anyhow::Result<Self> {
        let account = signed_in(account)?;
        // The rest is Claude's defaults: none of it is the pilot's to change
        let mut builder = claude::ConversationBuilder::from_config(&ClaudeConfig::default());
        builder
            .base_url(base_url(account))
            .bearer_source(Arc::new(Bearer(account.clone())))
            .server_settings()
            .system(system)
            .tools(tools);
        Ok(Self {
            inner: builder.build(client)?,
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
            self.inner
                .send_streaming(text, on_text)
                .await
                .map_err(readable_error)
        })
    }

    fn reset(&mut self) {
        self.inner.reset();
    }

    #[inline]
    fn usage(&self) -> &BTreeMap<String, Usage> {
        self.inner.usage()
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
    match error.downcast_ref::<claude::ApiError>().map(|e| e.status) {
        Some(StatusCode::UNAUTHORIZED) => error.context(t!("provider.bono_cloud_session_ended")),
        Some(StatusCode::FORBIDDEN) => error.context(t!("provider.bono_cloud_unavailable")),
        _ => error,
    }
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
                    "HTTP/1.1 {status} Mock\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
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

    fn rejected() -> (u16, String) {
        let error = json!({"type": "error", "error": {"type": "authentication_error", "message": "invalid token"}});
        (401, error.to_string())
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
            let mut responses = vec![(200, call.to_string())];
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
            let reply = conversation.send("Pit?".into()).await.unwrap();
            assert_eq!(reply.text, "Checking.\n\nBox.");
            let requests = requests.lock().unwrap();
            let last = requests.last().unwrap();
            assert!(last.0.contains("authorization: bearer access-new"));
            let messages = last.1["messages"].as_array().unwrap();
            assert_eq!(messages.len(), 3);
            // The signed thinking goes back as received
            assert_eq!(messages[1]["content"][0], thinking);
            assert_eq!(messages[1]["content"][2]["id"], "gap-1");
            assert_eq!(messages[2]["content"][0]["tool_use_id"], "gap-1");
            assert_eq!(messages[2]["content"][0]["content"], "Gap: 2s");
            if !expire {
                assert_eq!(requests[1].1, last.1);
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
