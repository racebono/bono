use crate::claude::tool_handler::{ToolHandler, Tools};
use anyhow::{Context, bail};
use futures::future::join_all;
use reqwest::{
    RequestBuilder, StatusCode,
    header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue},
};
use serde::{Deserialize, Serialize};
use std::{
    env::VarError,
    fmt::{self, Display, Formatter},
    num::NonZeroU32,
    time::{Duration, SystemTime},
};

mod tool_handler;

pub struct Conversation {
    client: reqwest::Client,
    base_url: String,
    body: RequestBody,
    headers: HeaderMap,
    timeout: Duration,
    max_retries: u32,
    max_requests: NonZeroU32,
    tools: Tools,
}

/// What Claude answered to one `Conversation::send`.
#[derive(Debug, Clone)]
pub struct Reply {
    /// Every text block of the turn, in order (with tool calls, Claude may write some before the
    /// calls and the rest after), separated by blank lines.
    pub text: String,
    pub stop: StopReason,
}

/// Why Claude's turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// It finished its answer.
    EndTurn,
    /// It wrote one of the request's stop sequences.
    StopSequence,
    /// The answer was cut off at `ConversationBuilder::max_tokens`: `text` is incomplete.
    MaxTokens,
    /// The conversation no longer fits in the model's context window: `text` is incomplete, and
    /// every later request would end the same way, so it needs to start over.
    ContextWindowExceeded,
    /// The model (and every fallback model) declined. The exchange isn't kept in the
    /// conversation, as if it never happened.
    Refusal,
}

impl Conversation {
    /// Sends the user's message and runs Claude's turn to its end, tool calls included. On an
    /// error, or if the returned future is dropped before it finishes, the conversation is left
    /// as it was before the call.
    pub async fn send(&mut self, text: String) -> anyhow::Result<Reply> {
        let mut turn = Turn {
            start: self.body.messages.len(),
            conversation: self,
            keep: false,
        };

        let reply = turn.conversation.run_turn(text).await?;
        turn.keep = reply.stop != StopReason::Refusal;
        return Ok(reply);
    }

    async fn run_turn(&mut self, text: String) -> anyhow::Result<Reply> {
        self.body.messages.push(RequestMessage {
            role: "user",
            content: serde_json::Value::String(text),
        });

        let mut texts = Vec::new();
        for _ in 0..self.max_requests.get() {
            let response = self.post().await?;
            if let Some(container_id) = response.container.and_then(|c| c.id) {
                self.body.container.get_or_insert_default().id = Some(container_id);
            }

            texts.extend(
                response
                    .content
                    .iter()
                    .filter(|block| block["type"] == "text")
                    .filter_map(|block| block["text"].as_str())
                    .filter(|text| !text.trim().is_empty())
                    .map(str::to_owned),
            );

            // Client tool calls, which this side answers (server tools like code execution run on
            // Anthropic's side)
            let calls = response
                .content
                .iter()
                .filter(|block| block["type"] == "tool_use")
                .cloned()
                .collect::<Vec<_>>();

            // The server stopped its own tool loop midway: the same request resumes it
            let paused = response.stop_reason.as_deref() == Some("pause_turn");
            let stop = match response.stop_reason.as_deref() {
                Some("tool_use") if !calls.is_empty() => None,
                Some("tool_use") => bail!("Claude stopped for tool calls, but made none"),
                Some("pause_turn") => None,
                Some("end_turn") => Some(StopReason::EndTurn),
                Some("stop_sequence") => Some(StopReason::StopSequence),
                Some("max_tokens") => Some(StopReason::MaxTokens),
                Some("model_context_window_exceeded") => Some(StopReason::ContextWindowExceeded),
                Some("refusal") => Some(StopReason::Refusal),
                other => bail!("Unexpected stop reason from Claude: {other:?}"),
            };

            // The assistant's turn goes into the history exactly as received: tool results must
            // follow the turn with their `tool_use` blocks, a `pause_turn` resumes from it, and
            // thinking blocks are only valid in an unedited history. Except when it ended with
            // tool calls that won't run (cut off, or declined): a `tool_use` without its result
            // would make every later request fail
            if !response.content.is_empty() && (stop.is_none() || calls.is_empty()) {
                self.body.messages.push(RequestMessage {
                    role: "assistant",
                    content: serde_json::Value::Array(response.content),
                });
            }

            if let Some(stop) = stop {
                return Ok(Reply {
                    text: texts.join("\n\n"),
                    stop,
                });
            }

            if paused {
                continue;
            }

            // Only once the turn is known to be whole: a `tool_use` cut off at `max_tokens` can
            // still parse, with part of its input missing
            let tools = &self.tools;
            let results = join_all(calls.iter().map(|call| {
                let name = call.get("name").and_then(serde_json::Value::as_str);
                let input = call.get("input");
                let handled = name.zip(input).map(|(name, input)| tools.handle(name, input));
                let tool_use_id = call["id"].clone();

                async move {
                    let (content, is_error) = match handled {
                        Some(handled) => match handled.await {
                            Ok(content) => (content, false),
                            Err(e) => (format!("{e:#}"), true),
                        },
                        None => ("Invalid tool call request".into(), true),
                    };

                    ResultBody {
                        r#type: "tool_result",
                        tool_use_id,
                        content,
                        is_error,
                    }
                }
            }))
            .await;

            self.body.messages.push(RequestMessage {
                role: "user",
                content: serde_json::to_value(results)?,
            });
        }

        bail!(
            "Claude didn't finish its turn within {} requests",
            self.max_requests
        )
    }

    async fn post(&self) -> anyhow::Result<ResponseBody> {
        const RETRY_AFTER: HeaderName = HeaderName::from_static("retry-after");
        const SHOULD_RETRY: HeaderName = HeaderName::from_static("x-should-retry");

        let mut attempt = 0;
        loop {
            let resp = match self.request().send().await {
                Ok(resp) => resp,
                // The request may never have reached the API: worth another try
                Err(e)
                    if attempt < self.max_retries
                        && (e.is_connect() || e.is_timeout() || e.is_request()) =>
                {
                    log::warn!("Claude request failed, retrying: {e}");
                    tokio::time::sleep(backoff(attempt)).await;
                    attempt += 1;
                    continue;
                }
                Err(e) => return Err(e).context("Claude request failed"),
            };

            let status = resp.status();
            if status.is_success() {
                return resp
                    .json()
                    .await
                    .context("Claude sent a response that couldn't be read");
            }

            // The API says whether retrying can help; without it, overloaded and transient errors
            let retryable = match resp.headers().get(SHOULD_RETRY).map(HeaderValue::as_bytes) {
                Some(b"true") => true,
                Some(b"false") => false,
                _ => {
                    status.is_server_error()
                        || matches!(
                            status,
                            StatusCode::REQUEST_TIMEOUT
                                | StatusCode::CONFLICT
                                | StatusCode::TOO_MANY_REQUESTS
                        )
                }
            };
            let retry_after = resp.headers().get(RETRY_AFTER).and_then(parse_retry_after);
            let error = ApiError::read(resp).await;

            if !retryable || attempt >= self.max_retries {
                return Err(error.into());
            }

            log::warn!("{error}, retrying");
            tokio::time::sleep(retry_after.unwrap_or_else(|| backoff(attempt))).await;
            attempt += 1;
        }
    }

    fn request(&self) -> RequestBuilder {
        return self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .headers(self.headers.clone())
            .timeout(self.timeout)
            .json(&self.body);
    }
}

/// Undoes a turn's changes to the history unless it's kept: on an error, a refusal, or the
/// future being dropped midway (which would otherwise leave a user message without an answer, or
/// tool calls without their results).
struct Turn<'a> {
    conversation: &'a mut Conversation,
    start: usize,
    keep: bool,
}

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        if !self.keep {
            self.conversation.body.messages.truncate(self.start);
        }
    }
}

/// An error the API answered with.
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    /// e.g. `invalid_request_error`, `authentication_error`, `overloaded_error`
    pub r#type: Option<String>,
    pub message: Option<String>,
    pub request_id: Option<String>,
}

impl ApiError {
    async fn read(resp: reqwest::Response) -> Self {
        const REQUEST_ID: HeaderName = HeaderName::from_static("request-id");

        #[derive(Deserialize)]
        struct Body {
            error: Detail,
        }

        #[derive(Deserialize)]
        struct Detail {
            r#type: Option<String>,
            message: Option<String>,
        }

        let status = resp.status();
        let request_id = resp
            .headers()
            .get(REQUEST_ID)
            .and_then(|id| id.to_str().ok())
            .map(str::to_owned);

        let body = resp.text().await.unwrap_or_default();
        let (r#type, message) = match serde_json::from_str::<Body>(&body) {
            Ok(body) => (body.error.r#type, body.error.message),
            Err(_) => (None, Some(body).filter(|body| !body.trim().is_empty())),
        };

        Self {
            status,
            r#type,
            message,
            request_id,
        }
    }
}

impl Display for ApiError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "Claude API error {}", self.status)?;
        if let Some(r#type) = &self.r#type {
            write!(f, " ({type})")?;
        }
        if let Some(message) = &self.message {
            write!(f, ": {message}")?;
        }
        if let Some(request_id) = &self.request_id {
            write!(f, " [request {request_id}]")?;
        }
        Ok(())
    }
}

impl std::error::Error for ApiError {}

pub struct ConversationBuilder {
    api_key: Option<String>,
    workspace_id: Option<String>,
    base_url: String,
    model: String,
    effort: String,
    max_tokens: NonZeroU32,
    system: String,
    tools: Tools,
    req_tools: Vec<RequestTool>,
    skills: Vec<()>,
    max_requests: NonZeroU32,
    max_retries: u32,
    timeout: Duration,
}

impl ConversationBuilder {
    pub fn new() -> Self {
        Self {
            api_key: Default::default(),
            workspace_id: Default::default(),
            base_url: "https://api.anthropic.com".into(),
            model: "claude-sonnet-5-5".into(),
            effort: "high".into(),
            max_tokens: NonZeroU32::new(16000).unwrap(),
            system: "".into(),
            tools: Tools::default(),
            req_tools: Vec::new(),
            skills: Vec::new(),
            max_requests: NonZeroU32::new(16).unwrap(),
            max_retries: 2,
            timeout: Duration::from_mins(10),
        }
    }

    /// Defaults to `ANTHROPIC_API_KEY`, then `ANTHROPIC_AUTH_TOKEN` (sent as a bearer token).
    #[inline]
    pub fn api_key(&mut self, api_key: impl Into<String>) -> &mut Self {
        self.api_key = Some(api_key.into());
        self
    }

    /// Defaults to `ANTHROPIC_WORKSPACE_ID`, if set.
    #[inline]
    pub fn workspace_id(&mut self, workspace_id: impl Into<String>) -> &mut Self {
        self.workspace_id = Some(workspace_id.into());
        self
    }

    /// Defaults to `https://api.anthropic.com`.
    #[inline]
    pub fn base_url(&mut self, base_url: impl Into<String>) -> &mut Self {
        self.base_url = base_url.into();
        self
    }

    /// Defaults to `claude-sonnet-5-5`.
    #[inline]
    pub fn model(&mut self, model: impl Into<String>) -> &mut Self {
        self.model = model.into();
        self
    }

    /// `low`, `medium`, `high`, `xhigh` or `max` (the levels depend on the model). Defaults to
    /// `high`.
    #[inline]
    pub fn effort(&mut self, effort: impl Into<String>) -> &mut Self {
        self.effort = effort.into();
        self
    }

    /// The most tokens Claude writes in one response, thinking included. Defaults to 16000.
    #[inline]
    pub fn max_tokens(&mut self, max_tokens: NonZeroU32) -> &mut Self {
        self.max_tokens = max_tokens;
        self
    }

    /// The system prompt. Cached apart from the conversation, so it's only paid in full when it
    /// changes. Defaults to none.
    #[inline]
    pub fn system(&mut self, system: impl Into<String>) -> &mut Self {
        self.system = system.into();
        self
    }

    /// The most requests one `Conversation::send` makes (one per round of tool calls).
    /// Defaults to 16.
    #[inline]
    pub fn max_requests(&mut self, max_requests: NonZeroU32) -> &mut Self {
        self.max_requests = max_requests;
        self
    }

    /// How often a failed request is retried, when retrying can help. Defaults to 2.
    #[inline]
    pub fn max_retries(&mut self, max_retries: u32) -> &mut Self {
        self.max_retries = max_retries;
        self
    }

    /// How long one request may take. Defaults to 10 minutes.
    #[inline]
    pub fn timeout(&mut self, timeout: Duration) -> &mut Self {
        self.timeout = timeout;
        self
    }

    #[inline]
    pub fn tool<H>(
        &mut self,
        name: &'static str,
        description: Option<String>,
        input_schema: serde_json::Value,
        handler: H,
    ) -> &mut Self
    where
        H: ToolHandler + 'static,
    {
        self.req_tools.push(RequestTool {
            r#type: None,
            name,
            description,
            input_schema,
        });
        self.tools.insert(name, handler);
        self
    }

    pub fn build(self, client: reqwest::Client) -> anyhow::Result<Conversation> {
        let (api_key, use_bearer) = match self.api_key {
            Some(api_key) => (api_key, false),
            None => match std::env::var("ANTHROPIC_API_KEY") {
                Ok(api_key) => (api_key, false),
                Err(VarError::NotPresent) => match std::env::var("ANTHROPIC_AUTH_TOKEN") {
                    Ok(auth_token) => (auth_token, true),
                    Err(VarError::NotPresent) => bail!(
                        "No Claude credentials: set ANTHROPIC_API_KEY or ConversationBuilder::api_key"
                    ),
                    Err(VarError::NotUnicode(s)) => {
                        bail!("environment variable was not valid unicode: {:?}", s)
                    }
                },
                Err(VarError::NotUnicode(s)) => {
                    bail!("environment variable was not valid unicode: {:?}", s)
                }
            },
        };

        let workspace_id = match self.workspace_id {
            Some(workspace_id) => Some(workspace_id),
            None => match std::env::var("ANTHROPIC_WORKSPACE_ID") {
                Ok(workspace_id) => Some(workspace_id),
                Err(VarError::NotPresent) => None,
                Err(VarError::NotUnicode(s)) => {
                    bail!("environment variable was not valid unicode: {:?}", s)
                }
            },
        };

        let mut betas = String::from("server-side-fallback-2026-07-01");
        if !self.skills.is_empty() {
            betas.push_str(",code-execution-2025-08-25");
        }

        const X_API_KEY: HeaderName = HeaderName::from_static("x-api-key");
        const ANTHROPIC_BETA: HeaderName = HeaderName::from_static("anthropic-beta");
        const ANTHROPIC_WORKSPACE_ID: HeaderName =
            HeaderName::from_static("anthropic-workspace-id");
        const ANTHROPIC_VERSION: HeaderName = HeaderName::from_static("anthropic-version");
        const ANTHROPIC_VERSION_VALUE: HeaderValue = HeaderValue::from_static("2023-06-01");

        let mut headers = HeaderMap::new();
        headers.insert(ANTHROPIC_VERSION, ANTHROPIC_VERSION_VALUE);

        if use_bearer {
            let bearer = format!("Bearer {api_key}");
            headers.insert(AUTHORIZATION, HeaderValue::from_str(&bearer)?);
            betas.push_str(",oauth-2025-04-20");
        } else {
            headers.insert(X_API_KEY, HeaderValue::from_str(&api_key)?);
        }

        if let Some(workspace_id) = workspace_id {
            headers.insert(
                ANTHROPIC_WORKSPACE_ID,
                HeaderValue::from_str(&workspace_id)?,
            );
        }

        // The system prompt gets its own cache breakpoint, so it's read from the cache even when
        // the conversation's (the top-level `cache_control`) misses
        let system = match self.system.trim().is_empty() {
            true => Vec::new(),
            false => vec![RequestSystemBlock {
                r#type: "text",
                text: self.system,
                cache_control: RequestCacheControl {
                    r#type: "ephemeral",
                },
            }],
        };

        headers.insert(ANTHROPIC_BETA, HeaderValue::from_str(&betas)?);
        return Ok(Conversation {
            client,
            base_url: self.base_url,
            body: RequestBody {
                model: self.model,
                max_tokens: self.max_tokens,
                fallbacks: "default",
                output_config: RequestOutputConfig {
                    effort: self.effort,
                },
                cache_control: RequestCacheControl {
                    r#type: "ephemeral",
                },
                system,
                messages: Vec::new(),
                tools: self.req_tools,
                container: None,
            },
            headers,
            timeout: self.timeout,
            max_retries: self.max_retries,
            max_requests: self.max_requests,
            tools: self.tools,
        });
    }
}

#[derive(Serialize)]
struct RequestBody {
    model: String,
    max_tokens: NonZeroU32,
    /// Retries refusals on Anthropic's recommended model for the refusal's category (needs the
    /// `server-side-fallback-2026-07-01` beta)
    fallbacks: &'static str,
    output_config: RequestOutputConfig,
    /// Automatic caching: a breakpoint on the conversation's last block, moving with it
    cache_control: RequestCacheControl,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    system: Vec<RequestSystemBlock>,
    messages: Vec<RequestMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<RequestTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    container: Option<RequestContainer>,
}

#[derive(Serialize)]
struct RequestSystemBlock {
    r#type: &'static str,
    text: String,
    cache_control: RequestCacheControl,
}

#[derive(Serialize)]
struct RequestMessage {
    role: &'static str,
    content: serde_json::Value,
}

#[derive(Serialize)]
struct RequestTool {
    #[serde(skip_serializing_if = "Option::is_none")]
    r#type: Option<&'static str>,
    name: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "serde_json::Value::is_null")]
    input_schema: serde_json::Value,
}

#[derive(Serialize)]
struct RequestSkill {
    r#type: String,
    skill_id: String,
    version: String,
}

#[derive(Default, Serialize)]
struct RequestContainer {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    skills: Vec<RequestSkill>,
}

#[derive(Serialize)]
struct RequestOutputConfig {
    effort: String,
}

#[derive(Serialize)]
struct RequestCacheControl {
    r#type: &'static str,
}

#[derive(Deserialize)]
struct ResponseBody {
    #[serde(default)]
    usage: ResponseUsage,
    #[serde(default)]
    container: Option<ResponseContainer>,
    #[serde(default)]
    stop_reason: Option<String>,
    /// Kept as raw JSON so it can be sent back unchanged (thinking blocks carry signatures)
    #[serde(default)]
    content: Vec<serde_json::Value>,
}

#[derive(Default, Deserialize)]
struct ResponseUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
}

#[derive(Deserialize)]
struct ResponseContainer {
    #[serde(default)]
    id: Option<String>,
}

#[derive(Serialize)]
struct ResultBody {
    r#type: &'static str,
    tool_use_id: serde_json::Value,
    content: String,
    is_error: bool,
}

/// 0.5s, 1s, 2s, ... with up to half of it added or taken away at random, so clients that failed
/// together don't all retry together.
fn backoff(attempt: u32) -> Duration {
    Duration::from_millis(500)
        .saturating_mul(1 << attempt.min(16))
        .mul_f64(rand::random_range(0.5..1.5))
}

fn parse_retry_after(value: &HeaderValue) -> Option<Duration> {
    let value = value.to_str().ok()?.trim();

    if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) {
        return Some(Duration::from_secs(value.parse::<u64>().ok()?));
    }

    let at = httpdate::parse_http_date(value).ok()?;
    return Some(
        at.duration_since(SystemTime::now())
            .unwrap_or(Duration::ZERO),
    );
}

/// `Conversation::send` can run on any of the runtime's threads (e.g. in `tokio::spawn`).
#[allow(dead_code)]
fn assert_send_is_send(conversation: &mut Conversation) {
    fn is_send<T: Send>(_: &T) {}
    is_send(&conversation.send(String::new()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        time::timeout,
    };

    /// Answers each request with the next of `responses` (status, extra headers, body), and
    /// records the request bodies. Past the last one, accepts connections and never answers.
    async fn mock(responses: Vec<(u16, &'static str, Value)>) -> (String, Arc<Mutex<Vec<Value>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();

        tokio::spawn(async move {
            let mut responses = responses.into_iter();
            let mut held = Vec::new();
            loop {
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
                recorded
                    .lock()
                    .unwrap()
                    .push(serde_json::from_slice(&buf[body_start..]).unwrap());

                let Some((status, headers, body)) = responses.next() else {
                    held.push(stream);
                    continue;
                };
                let body = body.to_string();
                let response = format!(
                    "HTTP/1.1 {status} Mock\r\ncontent-type: application/json\r\n{headers}content-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });

        (url, requests)
    }

    struct Lookup(Arc<AtomicUsize>);

    impl ToolHandler for Lookup {
        type Input = Value;

        fn handle(
            &self,
            input: Value,
        ) -> impl Future<Output = anyhow::Result<String>> + Send + 'static {
            self.0.fetch_add(1, Ordering::Relaxed);
            async move { Ok(format!("looked up {}", input["what"])) }
        }
    }

    fn conversation(url: &str, calls: &Arc<AtomicUsize>) -> Conversation {
        let mut builder = ConversationBuilder::new();
        builder
            .api_key("test")
            .base_url(url)
            .system("You are a race engineer.")
            .max_retries(2)
            .tool("lookup", None, json!({"type": "object"}), Lookup(calls.clone()));
        builder.build(reqwest::Client::new()).unwrap()
    }

    fn message(stop_reason: &str, content: Value) -> (u16, &'static str, Value) {
        (200, "", json!({"stop_reason": stop_reason, "content": content}))
    }

    async fn send(conversation: &mut Conversation, text: &str) -> anyhow::Result<Reply> {
        timeout(Duration::from_secs(5), conversation.send(text.into()))
            .await
            .expect("timed out")
    }

    #[tokio::test]
    async fn runs_tool_calls_and_returns_the_text() {
        let (url, requests) = mock(vec![
            message(
                "tool_use",
                json!([
                    {"type": "text", "text": "Checking the gap."},
                    {"type": "tool_use", "id": "t1", "name": "lookup", "input": {"what": "gap"}}
                ]),
            ),
            message("end_turn", json!([{"type": "text", "text": "You are 2.1s ahead."}])),
        ])
        .await;
        let calls = Arc::new(AtomicUsize::new(0));
        let mut conversation = conversation(&url, &calls);

        let reply = send(&mut conversation, "What is the gap?").await.unwrap();
        assert_eq!(reply.stop, StopReason::EndTurn);
        assert_eq!(reply.text, "Checking the gap.\n\nYou are 2.1s ahead.");
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(conversation.body.messages.len(), 4);

        let requests = requests.lock().unwrap();
        assert_eq!(requests[0]["system"][0]["text"], "You are a race engineer.");
        assert_eq!(requests[0]["system"][0]["cache_control"]["type"], "ephemeral");
        let result = &requests[1]["messages"][2]["content"][0];
        assert_eq!(result["type"], "tool_result");
        assert_eq!(result["tool_use_id"], "t1");
        assert_eq!(result["content"], "looked up \"gap\"");
    }

    #[tokio::test]
    async fn doesnt_run_tool_calls_that_were_cut_off() {
        let (url, _) = mock(vec![message(
            "max_tokens",
            json!([{"type": "tool_use", "id": "t1", "name": "lookup", "input": {}}]),
        )])
        .await;
        let calls = Arc::new(AtomicUsize::new(0));
        let mut conversation = conversation(&url, &calls);

        let reply = send(&mut conversation, "What is the gap?").await.unwrap();
        assert_eq!(reply.stop, StopReason::MaxTokens);
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        // The user's message stays, the unanswerable tool call doesn't
        assert_eq!(conversation.body.messages.len(), 1);
    }

    #[tokio::test]
    async fn forgets_refused_exchanges() {
        let (url, _) = mock(vec![message("refusal", json!([]))]).await;
        let calls = Arc::new(AtomicUsize::new(0));
        let mut conversation = conversation(&url, &calls);

        let reply = send(&mut conversation, "Something declined").await.unwrap();
        assert_eq!(reply.stop, StopReason::Refusal);
        assert!(conversation.body.messages.is_empty());
    }

    #[tokio::test]
    async fn reports_api_errors_and_forgets_the_exchange() {
        let (url, requests) = mock(vec![(
            400,
            "request-id: req_123\r\n",
            json!({"type": "error", "error": {"type": "invalid_request_error", "message": "bad"}}),
        )])
        .await;
        let calls = Arc::new(AtomicUsize::new(0));
        let mut conversation = conversation(&url, &calls);

        let error = send(&mut conversation, "Hi").await.unwrap_err();
        let error = error.downcast_ref::<ApiError>().unwrap();
        assert_eq!(error.status, StatusCode::BAD_REQUEST);
        assert_eq!(error.r#type.as_deref(), Some("invalid_request_error"));
        assert_eq!(error.message.as_deref(), Some("bad"));
        assert_eq!(
            error.to_string(),
            "Claude API error 400 Bad Request (invalid_request_error): bad [request req_123]"
        );
        assert!(conversation.body.messages.is_empty());
        // Not retried
        assert_eq!(requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn retries_overloaded_requests() {
        let (url, requests) = mock(vec![
            (
                529,
                "retry-after: 0\r\n",
                json!({"type": "error", "error": {"type": "overloaded_error", "message": "busy"}}),
            ),
            message("end_turn", json!([{"type": "text", "text": "Box this lap."}])),
        ])
        .await;
        let calls = Arc::new(AtomicUsize::new(0));
        let mut conversation = conversation(&url, &calls);

        let reply = send(&mut conversation, "Pit?").await.unwrap();
        assert_eq!(reply.text, "Box this lap.");
        assert_eq!(requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn forgets_cancelled_exchanges() {
        // Never answers
        let (url, _) = mock(Vec::new()).await;
        let calls = Arc::new(AtomicUsize::new(0));
        let mut conversation = conversation(&url, &calls);

        let cancelled = timeout(Duration::from_millis(200), conversation.send("Hi".into())).await;
        assert!(cancelled.is_err());
        assert!(conversation.body.messages.is_empty());
    }
}
