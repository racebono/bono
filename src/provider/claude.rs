//! Claude, through the Claude API.

use super::{Model, ModelOption, Reply, StopReason, ToolHandler, Tools, Usage, text::TextSink};
use crate::config::{ClaudeCacheTtl, ClaudeConfig};
use anyhow::{Context, bail};
use bono_i18n::t;
use futures::future::{BoxFuture, join_all};
use reqwest::{
    RequestBuilder, StatusCode,
    header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    env::VarError,
    fmt::{self, Display, Formatter},
    num::NonZeroU32,
    sync::Arc,
    time::{Duration, SystemTime},
};

/// Where the Claude API is, unless `ConversationBuilder::base_url` says otherwise.
const BASE_URL: &str = "https://api.anthropic.com";

/// The models the Claude API offers, newest first, asked with `config`'s credentials (or the
/// defaults, `ANTHROPIC_API_KEY`...).
pub async fn models(
    config: &ClaudeConfig,
    client: &reqwest::Client,
) -> anyhow::Result<Vec<ModelOption>> {
    #[derive(Deserialize)]
    struct Page {
        data: Vec<Entry>,
        #[serde(default)]
        has_more: bool,
        #[serde(default)]
        last_id: Option<String>,
    }

    #[derive(Deserialize)]
    struct Entry {
        id: String,
    }

    let headers = headers(
        config.api_key().clone(),
        config.workspace_id().clone(),
        String::new(),
    )?;
    let mut models = Vec::new();
    let mut after = None::<String>;
    loop {
        let mut url = format!("{BASE_URL}/v1/models?limit=1000");
        if let Some(after) = &after {
            url.push_str("&after_id=");
            url.push_str(after);
        }

        let resp = client
            .get(url)
            .headers(headers.clone())
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .context(t!("provider.claude_unreachable"))?;
        if !resp.status().is_success() {
            return Err(ApiError::read(resp).await.into());
        }

        let page = resp
            .json::<Page>()
            .await
            .context("Claude sent a list of models that couldn't be read")?;
        models.extend(page.data.into_iter().map(|entry| ModelOption {
            name: entry.id,
            size: None,
        }));
        match (page.has_more, page.last_id) {
            (true, Some(last)) => after = Some(last),
            _ => return Ok(models),
        }
    }
}

/// Whether there are credentials for the API: `config`'s key, `ANTHROPIC_API_KEY`, or
/// `ANTHROPIC_AUTH_TOKEN`.
pub fn has_credentials(config: &ClaudeConfig) -> bool {
    config.api_key().is_some()
        || ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"]
            .iter()
            .any(|var| std::env::var_os(var).is_some())
}

/// The API's headers: the credentials (`api_key`, or else `ANTHROPIC_API_KEY`, then
/// `ANTHROPIC_AUTH_TOKEN` as a bearer token), the workspace (or `ANTHROPIC_WORKSPACE_ID`), and
/// the `betas` (comma-separated).
fn headers(
    api_key: Option<String>,
    workspace_id: Option<String>,
    mut betas: String,
) -> anyhow::Result<HeaderMap> {
    let (api_key, use_bearer) = match api_key {
        Some(api_key) => (api_key, false),
        None => match std::env::var("ANTHROPIC_API_KEY") {
            Ok(api_key) => (api_key, false),
            Err(VarError::NotPresent) => match std::env::var("ANTHROPIC_AUTH_TOKEN") {
                Ok(auth_token) => (auth_token, true),
                Err(VarError::NotPresent) => bail!(t!("provider.no_claude_credentials")),
                Err(VarError::NotUnicode(s)) => {
                    bail!("environment variable was not valid unicode: {:?}", s)
                }
            },
            Err(VarError::NotUnicode(s)) => {
                bail!("environment variable was not valid unicode: {:?}", s)
            }
        },
    };

    let workspace_id = match workspace_id {
        Some(workspace_id) => Some(workspace_id),
        None => match std::env::var("ANTHROPIC_WORKSPACE_ID") {
            Ok(workspace_id) => Some(workspace_id),
            Err(VarError::NotPresent) => None,
            Err(VarError::NotUnicode(s)) => {
                bail!("environment variable was not valid unicode: {:?}", s)
            }
        },
    };

    const X_API_KEY: HeaderName = HeaderName::from_static("x-api-key");
    const ANTHROPIC_BETA: HeaderName = HeaderName::from_static("anthropic-beta");
    const ANTHROPIC_WORKSPACE_ID: HeaderName = HeaderName::from_static("anthropic-workspace-id");
    const ANTHROPIC_VERSION: HeaderName = HeaderName::from_static("anthropic-version");
    const ANTHROPIC_VERSION_VALUE: HeaderValue = HeaderValue::from_static("2023-06-01");

    let mut headers = HeaderMap::new();
    headers.insert(ANTHROPIC_VERSION, ANTHROPIC_VERSION_VALUE);

    if use_bearer {
        let bearer = format!("Bearer {api_key}");
        headers.insert(AUTHORIZATION, HeaderValue::from_str(&bearer)?);
        if !betas.is_empty() {
            betas.push(',');
        }
        betas.push_str("oauth-2025-04-20");
    } else {
        headers.insert(X_API_KEY, HeaderValue::from_str(&api_key)?);
    }

    if let Some(workspace_id) = workspace_id {
        headers.insert(
            ANTHROPIC_WORKSPACE_ID,
            HeaderValue::from_str(&workspace_id)?,
        );
    }

    if !betas.is_empty() {
        headers.insert(ANTHROPIC_BETA, HeaderValue::from_str(&betas)?);
    }
    Ok(headers)
}

/// A conversation with Claude.
pub struct Conversation {
    client: reqwest::Client,
    base_url: String,
    body: RequestBody,
    headers: HeaderMap,
    timeout: Duration,
    max_retries: u32,
    max_requests: NonZeroU32,
    tools: Tools,
    /// Every request's tokens, by the model that answered it
    usage: BTreeMap<String, Usage>,
}

/// Tokens billed, by kind, as the API counts them.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
struct Tokens {
    #[serde(default, deserialize_with = "null_as_default")]
    input_tokens: u64,
    #[serde(default, deserialize_with = "null_as_default")]
    cache_creation_input_tokens: u64,
    #[serde(default, deserialize_with = "null_as_default")]
    cache_read_input_tokens: u64,
    #[serde(default, deserialize_with = "null_as_default")]
    output_tokens: u64,
}

impl From<Tokens> for Usage {
    fn from(tokens: Tokens) -> Self {
        Self {
            input_tokens: tokens.input_tokens,
            cache_write_tokens: tokens.cache_creation_input_tokens,
            cache_read_tokens: tokens.cache_read_input_tokens,
            output_tokens: tokens.output_tokens,
        }
    }
}

/// Streamed counts can be `null` (e.g. the cache's, before they're known).
fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

impl Model for Conversation {
    fn send_streaming<'a>(
        &'a mut self,
        text: String,
        on_text: &'a mut (dyn FnMut(&str) + Send),
    ) -> BoxFuture<'a, anyhow::Result<Reply>> {
        Box::pin(async move {
            let mut turn = Turn {
                start: self.body.messages.len(),
                conversation: self,
                keep: false,
            };

            let reply = turn
                .conversation
                .run_turn(text, &mut TextSink::new(on_text))
                .await?;

            turn.keep = reply.stop != StopReason::Refusal;
            return Ok(reply);
        })
    }

    /// Also forgets the code execution container.
    fn reset(&mut self) {
        self.body.messages.clear();
        if let Some(container) = &mut self.body.container {
            container.id = None;
        }
    }

    #[inline]
    fn usage(&self) -> &BTreeMap<String, Usage> {
        &self.usage
    }
}

impl Conversation {
    /// A conversation with Claude as `config` says, with the defaults for the rest.
    pub fn new(
        config: &ClaudeConfig,
        system: Arc<str>,
        tools: Tools,
        client: reqwest::Client,
    ) -> anyhow::Result<Self> {
        let mut builder = ConversationBuilder::new();
        if let Some(api_key) = config.api_key() {
            builder.api_key(api_key);
        }
        if let Some(workspace_id) = config.workspace_id() {
            builder.workspace_id(workspace_id);
        }
        builder
            .model(config.model())
            .effort(config.effort())
            .cache_ttl(*config.cache_ttl())
            .max_tokens(*config.max_tokens())
            .compaction(*config.compaction())
            .system(system)
            .tools(tools);
        builder.build(client)
    }

    async fn run_turn(&mut self, text: String, sink: &mut TextSink<'_>) -> anyhow::Result<Reply> {
        self.body.messages.push(RequestMessage {
            role: "user",
            content: serde_json::Value::String(text),
        });

        let mut turn_usage = Usage::default();
        for _ in 0..self.max_requests.get() {
            let mut response = self.post(sink).await?;
            if let Some(container_id) = response.container.and_then(|c| c.id) {
                self.body.container.get_or_insert_default().id = Some(container_id);
            }

            let model = response.model.unwrap_or_else(|| self.body.model.clone());
            let usage = response.usage.total();
            turn_usage += usage;
            *self.usage.entry(model.clone()).or_default() += usage;

            drop_declined(&mut response.content);

            // Client tool calls, which this side answers (server tools like code execution run on
            // Anthropic's side)
            let calls = response
                .content
                .iter()
                .filter(|block| block["type"] == "tool_use")
                .cloned()
                .collect::<Vec<_>>();

            // The server stopped its own tool loop midway, or paused after compacting the
            // conversation: the same request carries on
            let paused = matches!(
                response.stop_reason.as_deref(),
                Some("pause_turn" | "compaction")
            );
            let stop = match response.stop_reason.as_deref() {
                Some("tool_use") if !calls.is_empty() => None,
                Some("tool_use") => bail!("Claude stopped for tool calls, but made none"),
                Some("pause_turn" | "compaction") => None,
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
                    text: std::mem::take(&mut sink.text),
                    stop,
                    usage: turn_usage,
                    model,
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
                let handled = name
                    .zip(input)
                    .map(|(name, input)| tools.handle(name, input));
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

    /// Sends the request and reads the response, streamed (handing its text to `sink` as it
    /// comes) or whole.
    async fn post(&self, sink: &mut TextSink<'_>) -> anyhow::Result<ResponseBody> {
        const RETRY_AFTER: HeaderName = HeaderName::from_static("retry-after");
        const SHOULD_RETRY: HeaderName = HeaderName::from_static("x-should-retry");

        let mut attempt = 0;
        loop {
            sink.start_response();
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
                let said = sink.text.len();
                match read_response(resp, sink).await {
                    Ok(body) => return Ok(body),
                    // Failed midway (e.g. overloaded, or the connection dropped): a retry would
                    // say again what's been said, so only before any text
                    Err(e)
                        if attempt < self.max_retries
                            && sink.text.len() == said
                            && retryable_stream_error(&e) =>
                    {
                        log::warn!("Claude's response failed midway, retrying: {e:#}");
                        tokio::time::sleep(backoff(attempt)).await;
                        attempt += 1;
                        continue;
                    }
                    Err(e) => return Err(e),
                }
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

    /// An `error` event in a streamed response, which arrives after the 200: the status is the
    /// one the error has when it's answered straight away.
    fn from_event(error: &serde_json::Value) -> Self {
        let r#type = error["type"].as_str().map(str::to_owned);
        let status = match r#type.as_deref() {
            Some("invalid_request_error") => StatusCode::BAD_REQUEST,
            Some("authentication_error") => StatusCode::UNAUTHORIZED,
            Some("billing_error") => StatusCode::PAYMENT_REQUIRED,
            Some("permission_error") => StatusCode::FORBIDDEN,
            Some("not_found_error") => StatusCode::NOT_FOUND,
            Some("request_too_large") => StatusCode::PAYLOAD_TOO_LARGE,
            Some("rate_limit_error") => StatusCode::TOO_MANY_REQUESTS,
            Some("timeout_error") => StatusCode::GATEWAY_TIMEOUT,
            Some("overloaded_error") => StatusCode::from_u16(529).unwrap(),
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };

        Self {
            status,
            r#type,
            message: error["message"].as_str().map(str::to_owned),
            request_id: None,
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
    cache_ttl: ClaudeCacheTtl,
    api_key: Option<String>,
    workspace_id: Option<String>,
    base_url: String,
    model: String,
    effort: String,
    max_tokens: NonZeroU32,
    system: Arc<str>,
    tools: Tools,
    /// The tools that run on Anthropic's side
    server_tools: Vec<RequestTool>,
    skills: Vec<RequestSkill>,
    compaction: bool,
    max_requests: NonZeroU32,
    max_retries: u32,
    timeout: Duration,
}

impl ConversationBuilder {
    pub fn new() -> Self {
        Self {
            cache_ttl: ClaudeCacheTtl::OneHour,
            api_key: Default::default(),
            workspace_id: Default::default(),
            base_url: BASE_URL.into(),
            model: "claude-sonnet-5-5".into(),
            effort: "high".into(),
            max_tokens: NonZeroU32::new(16000).unwrap(),
            system: Arc::default(),
            tools: Tools::default(),
            server_tools: Vec::new(),
            skills: Vec::new(),
            compaction: false,
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

    /// The lifetime of both prompt-cache breakpoints. Defaults to one hour.
    #[inline]
    pub fn cache_ttl(&mut self, ttl: ClaudeCacheTtl) -> &mut Self {
        self.cache_ttl = ttl;
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
    pub fn system(&mut self, system: impl Into<Arc<str>>) -> &mut Self {
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

    /// Has the API summarize the earlier conversation when it grows near the context window, so
    /// a long session never runs out of room. Only on models that support it (the Models API's
    /// `capabilities.context_management.compact_20260112`): others answer with an error. Off by
    /// default.
    #[inline]
    pub fn compaction(&mut self, compaction: bool) -> &mut Self {
        self.compaction = compaction;
        self
    }

    /// A tool that runs on Anthropic's side, e.g. `server_tool("web_search_20260209",
    /// "web_search")`.
    #[inline]
    pub fn server_tool(&mut self, r#type: &'static str, name: &'static str) -> &mut Self {
        self.server_tools.push(RequestTool {
            r#type: Some(r#type),
            name,
            description: None,
            input_schema: serde_json::Value::Null,
        });
        self
    }

    /// An Agent Skill, loaded into the code execution container when relevant: `r#type` is
    /// `anthropic` (Anthropic's own, e.g. `xlsx`) or `custom` (uploaded with the Skills API). The
    /// code execution tool is added with the first one. Skills run on Anthropic's side, so they
    /// can't reach files on this PC (the telemetry database included: see the engineer's tool).
    #[inline]
    pub fn skill(
        &mut self,
        r#type: impl Into<String>,
        skill_id: impl Into<String>,
        version: impl Into<String>,
    ) -> &mut Self {
        if self.skills.is_empty() {
            self.server_tool("code_execution_20260521", "code_execution");
        }
        self.skills.push(RequestSkill {
            r#type: r#type.into(),
            skill_id: skill_id.into(),
            version: version.into(),
        });
        self
    }

    /// A tool that runs on this side: see [`Tools::insert`].
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
        self.tools.insert(name, description, input_schema, handler);
        self
    }

    /// The tools that run on this side, replacing any added before.
    #[inline]
    pub fn tools(&mut self, tools: Tools) -> &mut Self {
        self.tools = tools;
        self
    }

    pub fn build(self, client: reqwest::Client) -> anyhow::Result<Conversation> {
        let mut betas = String::from("server-side-fallback-2026-07-01");
        if !self.skills.is_empty() {
            betas.push_str(",code-execution-2025-08-25");
        }
        if self.compaction {
            betas.push_str(",compact-2026-01-12");
        }

        let headers = headers(self.api_key, self.workspace_id, betas)?;

        // The system prompt gets its own cache breakpoint, so it's read from the cache even when
        // the conversation's (the top-level `cache_control`) misses
        let system = match self.system.trim().is_empty() {
            true => Vec::new(),
            false => vec![RequestSystemBlock {
                r#type: "text",
                text: self.system.clone(),
                cache_control: RequestCacheControl {
                    r#type: "ephemeral",
                    ttl: Some(self.cache_ttl.as_str()),
                },
            }],
        };

        let mut tools = self.server_tools;
        tools.extend(self.tools.definitions().map(|tool| RequestTool {
            r#type: None,
            name: tool.name,
            description: tool.description.map(str::to_owned),
            input_schema: tool.input_schema.clone(),
        }));

        return Ok(Conversation {
            client,
            base_url: self.base_url,
            body: RequestBody {
                model: self.model,
                max_tokens: self.max_tokens,
                stream: true,
                fallbacks: "default",
                output_config: RequestOutputConfig {
                    effort: self.effort,
                },
                cache_control: RequestCacheControl {
                    r#type: "ephemeral",
                    ttl: Some(self.cache_ttl.as_str()),
                },
                system,
                messages: Vec::new(),
                tools,
                container: (!self.skills.is_empty()).then(|| RequestContainer {
                    id: None,
                    skills: self.skills,
                }),
                context_management: self.compaction.then(|| RequestContextManagement {
                    edits: vec![RequestContextEdit {
                        r#type: "compact_20260112",
                    }],
                }),
            },
            headers,
            timeout: self.timeout,
            max_retries: self.max_retries,
            max_requests: self.max_requests,
            tools: self.tools,
            usage: BTreeMap::new(),
        });
    }
}

#[derive(Serialize)]
struct RequestBody {
    model: String,
    max_tokens: NonZeroU32,
    /// Always: answers are spoken as they're written, and long requests don't time out
    stream: bool,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    context_management: Option<RequestContextManagement>,
}

#[derive(Serialize)]
struct RequestContextManagement {
    edits: Vec<RequestContextEdit>,
}

#[derive(Serialize)]
struct RequestContextEdit {
    r#type: &'static str,
}

#[derive(Serialize)]
struct RequestSystemBlock {
    r#type: &'static str,
    text: Arc<str>,
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
    #[serde(skip_serializing_if = "Vec::is_empty")]
    skills: Vec<RequestSkill>,
}

#[derive(Serialize)]
struct RequestOutputConfig {
    effort: String,
}

#[derive(Serialize)]
struct RequestCacheControl {
    r#type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    ttl: Option<&'static str>,
}

#[derive(Deserialize)]
struct ResponseBody {
    #[serde(default)]
    model: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
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
    #[serde(flatten)]
    usage: Tokens,
    /// Present when the request ran server-side steps (e.g. compaction), each billed apart: the
    /// top-level counts are then only part of it
    #[serde(default, deserialize_with = "null_as_default")]
    iterations: Vec<Tokens>,
}

impl ResponseUsage {
    fn total(&self) -> Usage {
        match self.iterations.is_empty() {
            true => self.usage.into(),
            false => self
                .iterations
                .iter()
                .fold(Usage::default(), |mut total, usage| {
                    total += Usage::from(*usage);
                    total
                }),
        }
    }
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

/// A content block starts: a `fallback` block marks where a model declined midway, and the
/// fallback model's text carries on from the declined one's.
fn start_block(sink: &mut TextSink<'_>, block: &serde_json::Value) {
    sink.end_block();
    if block["type"] == "fallback" {
        sink.carry_on();
    }
}

/// Reads a response, streamed or whole, handing its text to `sink`.
async fn read_response(
    mut resp: reqwest::Response,
    sink: &mut TextSink<'_>,
) -> anyhow::Result<ResponseBody> {
    let streamed = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/event-stream"));

    if !streamed {
        let body = resp
            .json::<ResponseBody>()
            .await
            .context("Claude sent a response that couldn't be read")?;
        for block in &body.content {
            start_block(sink, block);
            if block["type"] == "text"
                && let Some(text) = block["text"].as_str()
            {
                sink.push(text);
            }
            sink.end_block();
        }
        return Ok(body);
    }

    let mut events = ServerEvents::default();
    let mut message = MessageStream::default();
    let mut data = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .context("Claude's response stream failed")?
    {
        events.feed(&chunk, &mut data);
        for data in data.drain(..) {
            message.event(&data, sink)?;
        }
        if message.done {
            break;
        }
    }

    return message.finish();
}

/// Splits a server-sent event stream into each event's data.
#[derive(Default)]
struct ServerEvents {
    /// The start of a line that's still coming
    line: Vec<u8>,
    /// The data of the event being read
    data: String,
}

impl ServerEvents {
    fn feed(&mut self, bytes: &[u8], events: &mut Vec<String>) {
        self.line.extend_from_slice(bytes);

        let mut start = 0;
        while let Some(len) = self.line[start..].iter().position(|&b| b == b'\n') {
            let line = &self.line[start..start + len];
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            start += len + 1;

            // A blank line ends the event
            if line.is_empty() {
                if !self.data.is_empty() {
                    events.push(std::mem::take(&mut self.data));
                }
                continue;
            }

            // The data says what the event is, so the `event:` lines aren't needed (nor
            // comments, `id:` or `retry:`)
            if let Some(value) = line.strip_prefix(b"data:") {
                let value = value.strip_prefix(b" ").unwrap_or(value);
                if !self.data.is_empty() {
                    self.data.push('\n');
                }
                self.data.push_str(&String::from_utf8_lossy(value));
            }
        }

        self.line.drain(..start);
    }
}

/// A message put together from its stream of events.
#[derive(Default)]
struct MessageStream {
    message: serde_json::Map<String, serde_json::Value>,
    blocks: Vec<serde_json::Value>,
    /// Each block's tool input so far, as JSON text
    inputs: Vec<String>,
    /// The message is complete
    done: bool,
}

impl MessageStream {
    fn event(&mut self, data: &str, sink: &mut TextSink<'_>) -> anyhow::Result<()> {
        use serde_json::Value;

        let mut event = serde_json::from_str::<Value>(data)
            .context("Claude sent a stream event that couldn't be read")?;

        match event["type"].as_str().unwrap_or_default() {
            "message_start" => {
                if let Value::Object(message) = event["message"].take() {
                    self.message = message;
                }
            }
            "content_block_start" => {
                let index = block_index(&event)?;
                let block = event["content_block"].take();
                if self.blocks.len() <= index {
                    self.blocks.resize(index + 1, Value::Null);
                    self.inputs.resize(index + 1, String::new());
                }

                start_block(sink, &block);
                if block["type"] == "text"
                    && let Some(text) = block["text"].as_str()
                {
                    sink.push(text);
                }
                self.blocks[index] = block;
            }
            "content_block_delta" => {
                let index = block_index(&event)?;
                let Some(block) = self.blocks.get_mut(index).filter(|block| block.is_object())
                else {
                    bail!("Claude streamed a delta for a block it never started");
                };
                let Value::Object(delta) = event["delta"].take() else {
                    return Ok(());
                };

                // Each delta's field goes onto the block's field of the same name: `text`,
                // `thinking`, `signature`, a compaction's `content`... Tool inputs are JSON,
                // complete only at the block's end
                let is_text = block["type"] == "text";
                for (key, value) in delta {
                    match (key.as_str(), value) {
                        ("type", _) => {}
                        ("partial_json", Value::String(json)) => self.inputs[index].push_str(&json),
                        ("citation", citation) => match &mut block["citations"] {
                            Value::Array(citations) => citations.push(citation),
                            other => *other = Value::Array(vec![citation]),
                        },
                        (key, Value::String(piece)) => {
                            if is_text && key == "text" {
                                sink.push(&piece);
                            }
                            match &mut block[key] {
                                Value::String(field) => field.push_str(&piece),
                                other => *other = Value::String(piece),
                            }
                        }
                        _ => {}
                    }
                }
            }
            "content_block_stop" => {
                let index = block_index(&event)?;
                if let Some(json) = self.inputs.get_mut(index).filter(|json| !json.is_empty()) {
                    match serde_json::from_str::<Value>(json) {
                        Ok(input) => self.blocks[index]["input"] = input,
                        // Cut off (at `max_tokens`): the call won't run
                        Err(e) => {
                            log::warn!("Claude streamed a tool input that couldn't be read: {e}")
                        }
                    }
                    json.clear();
                }
                sink.end_block();
            }
            "message_delta" => {
                // The stop reason, and the like
                if let Value::Object(delta) = event["delta"].take() {
                    self.message.extend(delta);
                }
                // The counts so far, replacing the ones before
                if let Value::Object(usage) = event["usage"].take() {
                    let usage = usage.into_iter().filter(|(_, value)| !value.is_null());
                    match self
                        .message
                        .entry("usage")
                        .or_insert_with(|| Value::Object(Default::default()))
                    {
                        Value::Object(total) => total.extend(usage),
                        other => *other = Value::Object(usage.collect()),
                    }
                }
            }
            "message_stop" => self.done = true,
            "error" => return Err(ApiError::from_event(&event["error"]).into()),
            // `ping`, and event types added later, which the API asks clients to ignore
            _ => {}
        }

        Ok(())
    }

    fn finish(mut self) -> anyhow::Result<ResponseBody> {
        if !self.done {
            return Err(StreamEnded.into());
        }

        let blocks = self.blocks.into_iter().filter(|block| !block.is_null());
        self.message
            .insert("content".into(), serde_json::Value::Array(blocks.collect()));
        serde_json::from_value(serde_json::Value::Object(self.message))
            .context("Claude sent a response that couldn't be read")
    }
}

fn block_index(event: &serde_json::Value) -> anyhow::Result<usize> {
    event["index"]
        .as_u64()
        .and_then(|index| usize::try_from(index).ok())
        .context("Claude streamed a block event without an index")
}

#[derive(Debug)]
struct StreamEnded;

impl Display for StreamEnded {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str("Claude's response stream ended before the message did")
    }
}

impl std::error::Error for StreamEnded {}

/// Whether a response that failed midway is worth asking for again.
fn retryable_stream_error(error: &anyhow::Error) -> bool {
    if let Some(error) = error.downcast_ref::<ApiError>() {
        return error.status.is_server_error()
            || matches!(
                error.status,
                StatusCode::REQUEST_TIMEOUT | StatusCode::TOO_MANY_REQUESTS
            );
    }
    if let Some(error) = error.downcast_ref::<reqwest::Error>() {
        return error.is_body() || error.is_decode() || error.is_timeout();
    }
    return error.is::<StreamEnded>();
}

/// When a model declined midway and a fallback model carried on (a `fallback` block marks
/// where), drops what the declined model started that can't go back in the history: its
/// thinking, its tool calls (which never run) and server tool calls without their results. Its
/// text stays: the fallback model carried on from it.
fn drop_declined(content: &mut Vec<serde_json::Value>) {
    let Some(boundary) = content
        .iter()
        .rposition(|block| block["type"] == "fallback")
    else {
        return;
    };

    let results = content
        .iter()
        .filter(|block| {
            block["type"]
                .as_str()
                .is_some_and(|r#type| r#type.ends_with("_tool_result"))
        })
        .filter_map(|block| block["tool_use_id"].as_str())
        .map(str::to_owned)
        .collect::<HashSet<_>>();

    let mut index = 0;
    content.retain(|block| {
        let keep = index >= boundary
            || match block["type"].as_str().unwrap_or_default() {
                "text" | "fallback" | "compaction" => true,
                "server_tool_use" | "mcp_tool_use" => {
                    block["id"].as_str().is_some_and(|id| results.contains(id))
                }
                r#type => r#type.ends_with("_tool_result"),
            };
        index += 1;
        keep
    });
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
                // A string is a stream of events (see `sse`)
                let (content_type, body) = match body {
                    Value::String(events) => ("text/event-stream", events),
                    body => ("application/json", body.to_string()),
                };
                let response = format!(
                    "HTTP/1.1 {status} Mock\r\ncontent-type: {content_type}\r\n{headers}content-length: {}\r\nconnection: close\r\n\r\n{body}",
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
            .tool(
                "lookup",
                None,
                json!({"type": "object"}),
                Lookup(calls.clone()),
            );
        builder.build(reqwest::Client::new()).unwrap()
    }

    fn message(stop_reason: &str, content: Value) -> (u16, &'static str, Value) {
        (
            200,
            "",
            json!({"stop_reason": stop_reason, "content": content}),
        )
    }

    /// A streamed response: each event as the API sends it, with CRLF line ends.
    fn sse(events: Vec<Value>) -> (u16, &'static str, Value) {
        let events = events
            .into_iter()
            .map(|event| {
                format!(
                    "event: {}\r\ndata: {event}\r\n\r\n",
                    event["type"].as_str().unwrap()
                )
            })
            .collect::<String>();
        (200, "", Value::String(events))
    }

    fn text_delta(index: usize, text: &str) -> Value {
        json!({"type": "content_block_delta", "index": index, "delta": {"type": "text_delta", "text": text}})
    }

    fn block_start(index: usize, block: Value) -> Value {
        json!({"type": "content_block_start", "index": index, "content_block": block})
    }

    fn block_stop(index: usize) -> Value {
        json!({"type": "content_block_stop", "index": index})
    }

    fn message_start(model: &str) -> Value {
        json!({"type": "message_start", "message": {
            "model": model, "role": "assistant", "content": [], "stop_reason": null,
            "usage": {"input_tokens": 10, "cache_creation_input_tokens": null, "cache_read_input_tokens": null, "output_tokens": 1}
        }})
    }

    fn message_end(stop_reason: &str, output_tokens: u64) -> [Value; 2] {
        [
            json!({"type": "message_delta", "delta": {"stop_reason": stop_reason, "stop_sequence": null}, "usage": {"output_tokens": output_tokens}}),
            json!({"type": "message_stop"}),
        ]
    }

    async fn send_streaming(
        conversation: &mut Conversation,
        text: &str,
    ) -> (anyhow::Result<Reply>, Vec<String>) {
        let mut pieces = Vec::new();
        let reply = timeout(
            Duration::from_secs(5),
            conversation.send_streaming(text.into(), &mut |piece| pieces.push(piece.to_owned())),
        )
        .await
        .expect("timed out");
        (reply, pieces)
    }

    #[tokio::test]
    async fn streams_the_text_as_it_comes() {
        let mut first = vec![
            message_start("claude-sonnet-5-5"),
            json!({"type": "ping"}),
            block_start(
                0,
                json!({"type": "thinking", "thinking": "", "signature": ""}),
            ),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "Need the "}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "gap."}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "sig"}}),
            block_stop(0),
            block_start(1, json!({"type": "text", "text": ""})),
            text_delta(1, "Checking"),
            text_delta(1, " the gap."),
            block_stop(1),
            block_start(
                2,
                json!({"type": "tool_use", "id": "t1", "name": "lookup", "input": {}}),
            ),
            json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "{\"what\": "}}),
            json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "\"gap\"}"}}),
            block_stop(2),
        ];
        first.extend(message_end("tool_use", 20));

        let mut second = vec![
            message_start("claude-sonnet-5-5"),
            // Says nothing: left out
            block_start(0, json!({"type": "text", "text": ""})),
            text_delta(0, "\n"),
            block_stop(0),
            block_start(1, json!({"type": "text", "text": ""})),
            text_delta(1, " "),
            text_delta(1, "You are 2.1s"),
            text_delta(1, " ahead."),
            block_stop(1),
        ];
        second.extend(message_end("end_turn", 7));

        let (url, requests) = mock(vec![sse(first), sse(second)]).await;
        let calls = Arc::new(AtomicUsize::new(0));
        let mut conversation = conversation(&url, &calls);

        let (reply, pieces) = send_streaming(&mut conversation, "What is the gap?").await;
        let reply = reply.unwrap();
        assert_eq!(reply.stop, StopReason::EndTurn);
        assert_eq!(reply.text, "Checking the gap.\n\nYou are 2.1s ahead.");
        assert_eq!(pieces.concat(), reply.text);
        assert_eq!(pieces[0], "Checking");
        assert_eq!(reply.usage.input_tokens, 20);
        assert_eq!(reply.usage.output_tokens, 27);
        assert_eq!(calls.load(Ordering::Relaxed), 1);

        let requests = requests.lock().unwrap();
        assert_eq!(requests[0]["stream"], true);
        // The streamed turn goes back whole: thinking with its signature, the tool input parsed
        let turn = &requests[1]["messages"][1]["content"];
        assert_eq!(turn[0]["thinking"], "Need the gap.");
        assert_eq!(turn[0]["signature"], "sig");
        assert_eq!(turn[1]["text"], "Checking the gap.");
        assert_eq!(turn[2]["input"], json!({"what": "gap"}));
        assert_eq!(
            requests[1]["messages"][2]["content"][0]["content"],
            "looked up \"gap\""
        );
    }

    #[tokio::test]
    async fn retries_streams_that_fail_before_saying_anything() {
        let (url, requests) = mock(vec![
            sse(vec![
                message_start("claude-sonnet-5-5"),
                json!({"type": "error", "error": {"type": "overloaded_error", "message": "busy"}}),
            ]),
            sse({
                let mut events = vec![
                    message_start("claude-sonnet-5-5"),
                    block_start(0, json!({"type": "text", "text": ""})),
                    text_delta(0, "Box this lap."),
                    block_stop(0),
                ];
                events.extend(message_end("end_turn", 5));
                events
            }),
        ])
        .await;
        let calls = Arc::new(AtomicUsize::new(0));
        let mut conversation = conversation(&url, &calls);

        let (reply, pieces) = send_streaming(&mut conversation, "Pit?").await;
        assert_eq!(reply.unwrap().text, "Box this lap.");
        assert_eq!(pieces, ["Box this lap."]);
        assert_eq!(requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn doesnt_retry_streams_that_fail_midway() {
        let (url, requests) = mock(vec![sse(vec![
            message_start("claude-sonnet-5-5"),
            block_start(0, json!({"type": "text", "text": ""})),
            text_delta(0, "Box"),
            json!({"type": "error", "error": {"type": "overloaded_error", "message": "busy"}}),
        ])])
        .await;
        let calls = Arc::new(AtomicUsize::new(0));
        let mut conversation = conversation(&url, &calls);

        let (reply, pieces) = send_streaming(&mut conversation, "Pit?").await;
        let error = reply.unwrap_err();
        assert_eq!(
            error.downcast_ref::<ApiError>().unwrap().r#type.as_deref(),
            Some("overloaded_error")
        );
        assert_eq!(pieces, ["Box"]);
        assert_eq!(requests.lock().unwrap().len(), 1);
        assert!(conversation.body.messages.is_empty());
    }

    #[tokio::test]
    async fn fails_streams_that_end_early() {
        let (url, _) = mock(vec![
            sse(vec![message_start("claude-sonnet-5-5")]),
            sse(vec![message_start("claude-sonnet-5-5")]),
            sse(vec![message_start("claude-sonnet-5-5")]),
        ])
        .await;
        let calls = Arc::new(AtomicUsize::new(0));
        let mut conversation = conversation(&url, &calls);

        let (reply, _) = send_streaming(&mut conversation, "Pit?").await;
        assert!(reply.unwrap_err().is::<StreamEnded>());
    }

    /// Against the real API: needs `ANTHROPIC_API_KEY`.
    #[tokio::test]
    #[ignore]
    async fn lists_the_models() {
        let models = super::models(&ClaudeConfig::default(), &reqwest::Client::new())
            .await
            .unwrap();
        println!("{models:#?}");
        assert!(models.iter().any(|model| model.name.starts_with("claude-")));
    }

    /// Needs `ANTHROPIC_API_KEY`, and spends a few tokens.
    #[tokio::test]
    #[ignore]
    async fn streams_from_the_api() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut builder = ConversationBuilder::new();
        builder
            .model("claude-haiku-5-5")
            .effort("low")
            .max_tokens(NonZeroU32::new(1024).unwrap())
            .system("You are a race engineer. Answer in one short sentence.")
            .tool(
                "lookup",
                Some("Looks up live race data, e.g. the gap".into()),
                json!({"type": "object", "properties": {"what": {"type": "string"}}, "required": ["what"]}),
                Lookup(calls.clone()),
            );
        let mut conversation = builder.build(reqwest::Client::new()).unwrap();

        let mut pieces = Vec::new();
        let reply = conversation
            .send_streaming("Look up the gap, then tell me it.".into(), &mut |piece| {
                pieces.push(piece.to_owned())
            })
            .await
            .unwrap();

        println!("{reply:#?}\n{pieces:?}");
        assert_eq!(reply.stop, StopReason::EndTurn);
        assert!(!reply.text.trim().is_empty());
        assert_eq!(pieces.concat(), reply.text);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert!(reply.usage.output_tokens > 0);
    }

    #[tokio::test]
    async fn carries_on_after_a_midway_fallback() {
        let mut events = vec![
            message_start("claude-fable-5-1"),
            block_start(
                0,
                json!({"type": "thinking", "thinking": "", "signature": ""}),
            ),
            block_stop(0),
            block_start(1, json!({"type": "text", "text": ""})),
            text_delta(1, "You are P3, the gap"),
            block_stop(1),
            block_start(
                2,
                json!({"type": "tool_use", "id": "t1", "name": "lookup", "input": {}}),
            ),
            block_stop(2),
            block_start(
                3,
                json!({"type": "fallback", "from": {"model": "claude-fable-5-1"}, "to": {"model": "claude-opus-5-5"}}),
            ),
            block_stop(3),
            block_start(4, json!({"type": "text", "text": ""})),
            text_delta(4, " behind is 2s."),
            block_stop(4),
        ];
        events.extend(message_end("end_turn", 9));

        let (url, requests) = mock(vec![
            sse(events),
            message("end_turn", json!([{"type": "text", "text": "Copy."}])),
        ])
        .await;
        let calls = Arc::new(AtomicUsize::new(0));
        let mut conversation = conversation(&url, &calls);

        let (reply, _) = send_streaming(&mut conversation, "Gap?").await;
        let reply = reply.unwrap();
        // The fallback model carried on mid-sentence
        assert_eq!(reply.text, "You are P3, the gap behind is 2s.");
        // The declined model's tool call never runs
        assert_eq!(calls.load(Ordering::Relaxed), 0);

        send(&mut conversation, "Thanks").await.unwrap();
        let requests = requests.lock().unwrap();
        let types = requests[1]["messages"][1]["content"]
            .as_array()
            .unwrap()
            .iter()
            .map(|block| block["type"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(types, ["text", "fallback", "text"]);
    }

    #[test]
    fn configured_cache_ttl_applies_to_both_breakpoints() {
        for (value, expected) in [(None, "1h"), (Some("5m"), "5m"), (Some("1h"), "1h")] {
            let mut json = json!({"api_key": "test-key"});
            if let Some(value) = value {
                json["cache_ttl"] = json!(value);
            }
            let config: ClaudeConfig = serde_json::from_value(json).unwrap();
            let conversation = Conversation::new(
                &config,
                "You are a race engineer.".into(),
                Tools::default(),
                reqwest::Client::new(),
            )
            .unwrap();
            let body = serde_json::to_value(&conversation.body).unwrap();
            let control = json!({"type": "ephemeral", "ttl": expected});
            assert_eq!(body["cache_control"], control);
            assert_eq!(body["system"][0]["cache_control"], control);
            let saved = serde_json::to_value(&config).unwrap();
            let restored: ClaudeConfig = serde_json::from_value(saved).unwrap();
            assert_eq!(restored.cache_ttl().as_str(), expected);
        }
        assert!(serde_json::from_value::<ClaudeConfig>(json!({"cache_ttl": "10m"})).is_err());
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
            message(
                "end_turn",
                json!([{"type": "text", "text": "You are 2.1s ahead."}]),
            ),
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
        assert_eq!(
            requests[0]["system"][0]["cache_control"],
            json!({"type": "ephemeral", "ttl": "1h"})
        );
        for request in requests.iter() {
            assert_eq!(
                request["cache_control"],
                json!({"type": "ephemeral", "ttl": "1h"})
            );
            assert_eq!(
                request["system"][0]["cache_control"],
                request["cache_control"]
            );
        }
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
            message(
                "end_turn",
                json!([{"type": "text", "text": "Box this lap."}]),
            ),
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

    #[tokio::test]
    async fn counts_usage_by_model() {
        let (url, _) = mock(vec![
            (
                200,
                "",
                json!({
                    "model": "claude-sonnet-5-5",
                    "stop_reason": "tool_use",
                    "content": [{"type": "tool_use", "id": "t1", "name": "lookup", "input": {}}],
                    "usage": {"input_tokens": 100, "cache_read_input_tokens": 50, "output_tokens": 10}
                }),
            ),
            (
                200,
                "",
                json!({
                    "model": "claude-opus-5-5",
                    "stop_reason": "end_turn",
                    "content": [{"type": "text", "text": "Done."}],
                    // Billed as two steps (e.g. compaction, then the answer)
                    "usage": {
                        "input_tokens": 1, "output_tokens": 1,
                        "iterations": [
                            {"input_tokens": 200, "output_tokens": 30},
                            {"input_tokens": 20, "cache_creation_input_tokens": 5, "output_tokens": 7}
                        ]
                    }
                }),
            ),
        ])
        .await;
        let calls = Arc::new(AtomicUsize::new(0));
        let mut conversation = conversation(&url, &calls);

        let reply = send(&mut conversation, "Hi").await.unwrap();
        assert_eq!(reply.model, "claude-opus-5-5");
        assert_eq!(
            reply.usage,
            Usage {
                input_tokens: 320,
                cache_write_tokens: 5,
                cache_read_tokens: 50,
                output_tokens: 47,
            }
        );
        assert_eq!(conversation.usage()["claude-sonnet-5-5"].input_tokens, 100);
        assert_eq!(conversation.usage()["claude-opus-5-5"].input_tokens, 220);
    }

    #[tokio::test]
    async fn sends_skills_and_compaction() {
        let (url, requests) = mock(vec![
            (
                200,
                "",
                json!({
                    "stop_reason": "end_turn",
                    "container": {"id": "container_1"},
                    "content": [{"type": "text", "text": "Hi."}]
                }),
            ),
            message("end_turn", json!([{"type": "text", "text": "Hi again."}])),
        ])
        .await;
        let mut builder = ConversationBuilder::new();
        builder
            .api_key("test")
            .base_url(&url)
            .skill("anthropic", "xlsx", "latest")
            .compaction(true);
        let mut conversation = builder.build(reqwest::Client::new()).unwrap();

        let betas = conversation.headers["anthropic-beta"].to_str().unwrap();
        assert!(betas.contains("code-execution-2025-08-25"), "{betas}");
        assert!(betas.contains("compact-2026-01-12"), "{betas}");

        send(&mut conversation, "Hi").await.unwrap();
        // A new session: the container goes with the messages, the skills stay
        conversation.reset();
        assert!(conversation.body.messages.is_empty());
        send(&mut conversation, "Hi").await.unwrap();

        let requests = requests.lock().unwrap();
        let first = &requests[0];
        assert_eq!(first["tools"][0]["type"], "code_execution_20260521");
        assert_eq!(first["tools"][0]["name"], "code_execution");
        assert_eq!(first["container"]["skills"][0]["skill_id"], "xlsx");
        assert!(first["container"].get("id").is_none());
        assert_eq!(
            first["context_management"]["edits"][0]["type"],
            "compact_20260112"
        );
        assert!(requests[1]["container"].get("id").is_none());
        assert_eq!(requests[1]["container"]["skills"][0]["skill_id"], "xlsx");
        assert_eq!(requests[1]["messages"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn carries_on_after_a_compaction_pause() {
        let (url, requests) = mock(vec![
            message(
                "compaction",
                json!([{"type": "compaction", "content": "Summary of the stint."}]),
            ),
            message(
                "end_turn",
                json!([{"type": "text", "text": "P3, holding."}]),
            ),
        ])
        .await;
        let calls = Arc::new(AtomicUsize::new(0));
        let mut conversation = conversation(&url, &calls);

        let reply = send(&mut conversation, "Position?").await.unwrap();
        assert_eq!(reply.text, "P3, holding.");
        // The compaction block goes back unchanged
        let requests = requests.lock().unwrap();
        assert_eq!(
            requests[1]["messages"][1]["content"][0]["type"],
            "compaction"
        );
    }
}
