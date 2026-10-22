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
    openai::models(Some(base_url(account).as_str()), Some(&token), client).await
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
    account: Account,
    inner: openai::Conversation,
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
        Ok(Self {
            account: account.clone(),
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
            // Access tokens last an hour: a fresh one for each question
            let token = access_token(&self.account).await?;
            self.inner.set_api_key(&token)?;
            self.inner.send_streaming(text, on_text).await
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
        .with_context(|| t!("provider.bono_cloud_sign_in"))
}

fn base_url(account: &Account) -> String {
    account.url(PATH).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{account::tests::signed_in, provider::StopReason};
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
