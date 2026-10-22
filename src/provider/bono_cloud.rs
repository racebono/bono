//! Bono Cloud: models Bono's website runs for signed-in accounts that have access to it. Which
//! model, whose, and with what settings is the website's choice, so there's nothing to set up
//! here: the website speaks an OpenAI-compatible chat completions API at [`PATH`], with the
//! account's access token as the bearer token, and answers `403` to accounts without access.

use super::{Model, ModelOption, Reply, Tools, Usage, openai};
use crate::account::{Account, Status};
use anyhow::Context;
use bono_i18n::t;
use futures::future::BoxFuture;
use reqwest::StatusCode;
use std::{collections::BTreeMap, sync::Arc};

/// Where the API is, on the account's website
const PATH: &str = "/api/cloud/v1";

/// The models Bono Cloud offers `account`.
pub async fn models(
    account: Option<&Account>,
    client: &reqwest::Client,
) -> anyhow::Result<Vec<ModelOption>> {
    let account = signed_in(account)?;
    let token = access_token(account).await?;
    let result = openai::models(Some(base_url(account).as_str()), Some(&token), client).await;
    let result = if result.as_ref().is_err_and(|error| unauthorized(error)) {
        let token = refresh_token(account, &token).await?;
        openai::models(Some(base_url(account).as_str()), Some(&token), client).await
    } else {
        result
    };
    result.map_err(readable_error)
}

/// Whether `account` may use Bono Cloud, as the website says.
pub async fn authorized(account: &Account, client: &reqwest::Client) -> anyhow::Result<bool> {
    match models(Some(account), client).await {
        Ok(_) => Ok(true),
        Err(e)
            if e.downcast_ref::<openai::ApiError>()
                .is_some_and(|e| e.status == StatusCode::FORBIDDEN) =>
        {
            Ok(false)
        }
        Err(e) => Err(e),
    }
}

/// A conversation with Bono Cloud, as `account`.
pub struct Conversation {
    inner: openai::Conversation,
}

struct Bearer(Account);

impl openai::BearerSource for Bearer {
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
        let mut builder = openai::ConversationBuilder::new();
        // The model and its settings are the website's: none are asked for
        builder
            .base_url(base_url(account))
            .system(system)
            .tools(tools);
        let mut inner = builder.build(client)?;
        inner.set_bearer_source(Arc::new(Bearer(account.clone())));
        Ok(Self { inner })
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
        .downcast_ref::<openai::ApiError>()
        .is_some_and(|error| error.status == StatusCode::UNAUTHORIZED)
}

fn readable_error(error: anyhow::Error) -> anyhow::Error {
    match error.downcast_ref::<openai::ApiError>().map(|e| e.status) {
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

    /// Answers each request with the next of `responses` (status and body), and records each
    /// request's head (lowercased) and body.
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

                let response = format!(
                    "HTTP/1.1 {status} Mock\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });

        (url, requests)
    }

    #[tokio::test]
    async fn asks_the_website_whether_the_account_may_use_it() {
        let (url, requests) = mock(vec![
            (200, json!({"data": [{"id": "bono"}]}).to_string()),
            (403, json!({"error": "forbidden"}).to_string()),
        ])
        .await;
        let account = signed_in(url, "access-1");
        let client = reqwest::Client::new();

        assert!(authorized(&account, &client).await.unwrap());
        assert!(!authorized(&account, &client).await.unwrap());

        let requests = requests.lock().unwrap();
        assert!(requests[0].0.starts_with("get /api/cloud/v1/models "));
        assert!(requests[0].0.contains("authorization: bearer access-1"));
    }

    #[tokio::test]
    async fn talks_as_the_account_without_settings() {
        let text = json!({"model": "bono", "choices": [{"index": 0, "delta": {"content": "Box."}, "finish_reason": "stop"}]});
        let (url, requests) = mock(vec![(200, format!("data: {text}\n\ndata: [DONE]\n\n"))]).await;
        let account = signed_in(url, "access-1");
        let mut conversation = Conversation::new(
            Some(&account),
            "You are a race engineer.".into(),
            Tools::default(),
            reqwest::Client::new(),
        )
        .unwrap();

        let reply = conversation.send("Pit?".into()).await.unwrap();
        assert_eq!(reply.text, "Box.");
        assert_eq!(reply.stop, StopReason::EndTurn);
        assert_eq!(reply.model, "bono");

        let requests = requests.lock().unwrap();
        let (head, body) = &requests[0];
        assert!(head.starts_with("post /api/cloud/v1/chat/completions "));
        assert!(head.contains("authorization: bearer access-1"));
        // The website picks the model and its settings
        assert_eq!(body["model"], "");
        assert!(body.get("reasoning_effort").is_none());
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
        let chunk = json!({"model": "bono", "choices": [{"delta": {"content": "Box."}, "finish_reason": "stop"}]});
        (200, format!("data: {chunk}\n\ndata: [DONE]\n\n"))
    }

    fn rejected() -> (u16, String) {
        (401, json!({"error": "invalid_token"}).to_string())
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
        assert_eq!(requests[2].1["messages"].as_array().unwrap().len(), 2);
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
        let (url, requests) = mock(vec![
            rejected(),
            (200, tokens().to_string()),
            (200, json!({"data": [{"id": "bono"}]}).to_string()),
        ])
        .await;
        let account = signed_in(url, "access-old");
        assert!(authorized(&account, &reqwest::Client::new()).await.unwrap());
        let requests = requests.lock().unwrap();
        assert!(requests[0].0.starts_with("get /api/cloud/v1/models "));
        assert!(requests[2].0.starts_with("get /api/cloud/v1/models "));
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
            (403, json!({"error": "forbidden"}).to_string()),
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
            let chunk = json!({"model": "bono", "choices": [{"delta": {
                "content": "Checking.",
                "tool_calls": [{"index": 0, "id": "gap-1", "function": {"name": "gap", "arguments": "{}"}}]
            }, "finish_reason": "tool_calls"}]});
            let mut responses = vec![(200, format!("data: {chunk}\n\ndata: [DONE]\n\n"))];
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
            assert_eq!(messages.len(), 4);
            assert_eq!(messages[2]["tool_calls"][0]["id"], "gap-1");
            assert_eq!(messages[3]["tool_call_id"], "gap-1");
            assert_eq!(messages[3]["content"], "Gap: 2s");
            if !expire {
                assert_eq!(requests[1].1, last.1);
            }
        }
    }

    #[tokio::test]
    async fn never_retries_a_failed_stream_after_emitting_text() {
        let chunk = json!({"model": "bono", "choices": [{"delta": {"content": "Box."}}]});
        let (url, requests) = mock(vec![(200, format!("data: {chunk}\n\n"))]).await;
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
