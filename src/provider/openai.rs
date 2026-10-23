//! Models behind an OpenAI-compatible chat completions API (`/v1/chat/completions`): llama.cpp's
//! `llama-server` (started with `--jinja`, for tool calls), LM Studio, vLLM, LocalAI, OpenRouter,
//! OpenAI itself...

use super::{Model, ModelOption, Reply, StopReason, ToolHandler, Tools, Usage, text::TextSink};
use crate::config::OpenAiConfig;
use anyhow::{Context, bail};
use bono_i18n::t;
use futures::future::{BoxFuture, join_all};
use reqwest::{
    StatusCode,
    header::{AUTHORIZATION, HeaderMap, HeaderValue},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    env::VarError,
    fmt::{self, Display, Formatter},
    num::NonZeroU32,
    sync::Arc,
    time::Duration,
};

/// Where the API is unless the config says otherwise: llama.cpp's `llama-server` on this PC.
pub const DEFAULT_BASE_URL: &str = "http://127.0.0.1:8080/v1";

/// The models the API at `base_url` (the default when `None`) serves.
pub async fn models(
    base_url: Option<&str>,
    api_key: Option<&str>,
    client: &reqwest::Client,
) -> anyhow::Result<Vec<ModelOption>> {
    #[derive(Deserialize)]
    struct Models {
        #[serde(default)]
        data: Vec<Entry>,
    }

    #[derive(Deserialize)]
    struct Entry {
        id: String,
    }

    let base_url = base_url_or_default(base_url);
    let resp = client
        .get(format!("{base_url}/models"))
        .headers(headers(api_key.map(str::to_owned))?)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .with_context(|| t!("provider.openai_unreachable", url = base_url))?;
    if !resp.status().is_success() {
        return Err(ApiError::read(resp).await.into());
    }

    let mut models = resp
        .json::<Models>()
        .await
        .context("The server sent a list of models that couldn't be read")?
        .data
        .into_iter()
        .map(|entry| ModelOption {
            name: entry.id,
            size: None,
        })
        .collect::<Vec<_>>();
    models.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(models)
}

/// Credentials supplied by providers whose bearer tokens can expire.
pub(super) trait BearerSource: Send + Sync {
    /// Obtain a bearer for each request, or replace one rejected before streaming began.
    fn token<'a>(&'a self, rejected: Option<&'a str>) -> BoxFuture<'a, anyhow::Result<String>>;
}

/// A conversation with a model behind an OpenAI-compatible API.
pub struct Conversation {
    client: reqwest::Client,
    base_url: String,
    headers: HeaderMap,
    bearer_source: Option<Arc<dyn BearerSource>>,
    body: RequestBody,
    native_history: bool,
    history_limits: Option<HistoryLimits>,
    /// The messages kept through a reset: the system prompt's
    kept: usize,
    timeout: Duration,
    max_requests: NonZeroU32,
    tools: Tools,
    /// Every turn's tokens, by the model that answered it
    usage: BTreeMap<String, Usage>,
}

impl Model for Conversation {
    fn send_streaming<'a>(
        &'a mut self,
        text: String,
        on_text: &'a mut (dyn FnMut(&str) + Send),
    ) -> BoxFuture<'a, anyhow::Result<Reply>> {
        Box::pin(async move {
            let original = self.history_limits.map(|_| self.body.messages.clone());
            let mut turn = Turn {
                start: self.body.messages.len(),
                conversation: self,
                keep: false,
                original,
            };
            let mut sink = TextSink::new(on_text);
            if let Some(limits) = turn.conversation.history_limits {
                turn.conversation.prepare_history(&text, limits.target)?;
            }
            let mut result = turn.conversation.run_turn(text.clone(), &mut sink).await;
            // A limit may be reached within a tool loop, too. Roll that entire attempt back
            // before retrying; never edit the signed history of a live tool loop.
            for attempt in 0..2 {
                if !result.as_ref().is_err_and(history_limit_error)
                    || turn.conversation.history_limits.is_none()
                {
                    break;
                }
                turn.conversation.body.messages = turn.original.as_ref().unwrap().clone();
                if attempt == 0 {
                    turn.conversation.prepare_history(&text, 0.25)?;
                } else {
                    log::warn!(
                        "Bono Cloud: resetting conversation after repeated history limit errors"
                    );
                    turn.conversation.reset();
                }
                result = turn.conversation.run_turn(text.clone(), &mut sink).await;
            }
            let reply = result?;
            turn.keep = reply.stop != StopReason::Refusal;
            Ok(reply)
        })
    }

    fn reset(&mut self) {
        self.body.messages.truncate(self.kept);
    }

    #[inline]
    fn usage(&self) -> &BTreeMap<String, Usage> {
        &self.usage
    }
}

impl Conversation {
    /// A conversation as `config` says, with the defaults for the rest.
    pub fn new(
        config: &OpenAiConfig,
        system: Arc<str>,
        tools: Tools,
        client: reqwest::Client,
    ) -> anyhow::Result<Self> {
        let mut builder = ConversationBuilder::new();
        if let Some(base_url) = config.base_url() {
            builder.base_url(base_url);
        }
        if let Some(api_key) = config.api_key() {
            builder.api_key(api_key);
        }
        builder
            .model(config.model())
            .effort(config.effort())
            .max_tokens(*config.max_tokens())
            .system(system)
            .tools(tools);
        builder.build(client)
    }

    /// Preserve Bono Cloud's signed native history extension. Thinking stays out of TextSink.
    pub(super) fn enable_native_history(&mut self) {
        self.native_history = true;
    }

    /// Supplies expiring credentials per request, including tool-call round trips.
    pub(super) fn set_bearer_source(&mut self, source: Arc<dyn BearerSource>) {
        self.bearer_source = Some(source);
    }

    /// Trim only between complete user turns. The system prefix and newest turn survive.
    fn prepare_history(&mut self, question: &str, target: f64) -> anyhow::Result<()> {
        let Some(limits) = self.history_limits else {
            return Ok(());
        };
        let question_bytes = serde_json::to_vec(&serde_json::json!({
            "role": "user", "content": question
        }))?
        .len()
            + 1;
        let size = |this: &Self| -> anyhow::Result<usize> {
            Ok(serde_json::to_vec(&this.body)?.len() + question_bytes)
        };
        if target == limits.target
            && size(self)? <= limits.max_bytes
            && self.body.messages.len() < limits.max_messages
        {
            return Ok(());
        }
        let before = self.body.messages.len();
        let starts: Vec<_> = self
            .body
            .messages
            .iter()
            .enumerate()
            .filter(|(_, message)| message["role"] == "user" && message["content"].is_string())
            .map(|(index, _)| index)
            .collect();
        let recent = starts.iter().rev().nth(3).copied().unwrap_or(self.kept);
        for message in &mut self.body.messages[self.kept..recent] {
            if message["role"] == "user" {
                if let Some(text) = message["content"].as_str() {
                    message["content"] = serde_json::Value::String(strip_snapshot(text));
                }
            }
        }
        while size(self)? > (limits.max_bytes as f64 * target) as usize
            || self.body.messages.len() + 1 > (limits.max_messages as f64 * target) as usize
        {
            let next = self
                .body
                .messages
                .iter()
                .enumerate()
                .skip(self.kept + 1)
                .find(|(_, message)| message["role"] == "user" && message["content"].is_string())
                .map(|(index, _)| index);
            let Some(next) = next else { break };
            self.body.messages.drain(self.kept..next);
        }
        log::info!(
            "Bono Cloud: forgot {} older messages to stay within limits",
            before - self.body.messages.len()
        );
        Ok(())
    }

    async fn run_turn(&mut self, text: String, sink: &mut TextSink<'_>) -> anyhow::Result<Reply> {
        self.body.messages.push(serde_json::json!({
            "role": "user",
            "content": text,
        }));

        let mut turn_usage = Usage::default();
        for _ in 0..self.max_requests.get() {
            sink.start_response();
            let response = self.post(sink).await?;
            sink.end_block();

            let model = response.model.unwrap_or_else(|| self.body.model.clone());
            turn_usage += response.usage;
            *self.usage.entry(model.clone()).or_default() += response.usage;

            let calls = response.tool_calls;
            let stop = match response.finish_reason.as_deref() {
                // Some servers say "stop" after tool calls
                Some("tool_calls" | "function_call" | "stop") if !calls.is_empty() => None,
                Some("tool_calls" | "function_call") => {
                    bail!("The model stopped for tool calls, but made none")
                }
                Some("stop") => Some(StopReason::EndTurn),
                Some("length") => Some(StopReason::MaxTokens),
                Some("content_filter") => Some(StopReason::Refusal),
                other => bail!("Unexpected stop reason from the model: {other:?}"),
            };

            // Except when it ended with tool calls that won't run (cut off): a call without its
            // result makes every later request fail
            let said = !response.content.is_empty() || !calls.is_empty();
            if said && (stop.is_none() || calls.is_empty()) {
                let mut message = serde_json::json!({
                    "role": "assistant",
                    "content": response.content,
                });
                if !calls.is_empty() {
                    message["tool_calls"] = calls
                        .iter()
                        .map(|call| {
                            serde_json::json!({
                                "id": call.id,
                                "type": "function",
                                "function": {"name": call.name, "arguments": call.arguments},
                            })
                        })
                        .collect();
                }
                if self.native_history {
                    if let Some(blocks) = response.bono_content {
                        message["bono_content"] = serde_json::Value::Array(blocks);
                    }
                }
                self.body.messages.push(message);
            }

            if let Some(stop) = stop {
                return Ok(Reply {
                    text: std::mem::take(&mut sink.text),
                    stop,
                    usage: turn_usage,
                    model,
                });
            }

            let tools = &self.tools;
            let result_limit = self.history_limits.map(|limits| limits.max_bytes / 8);
            let results = join_all(calls.iter().map(|call| {
                // The arguments are JSON text, empty for none
                let input = match call.arguments.trim() {
                    "" => Ok(serde_json::json!({})),
                    json => serde_json::from_str::<serde_json::Value>(json),
                };
                let handled = input.map(|input| tools.handle(&call.name, &input));
                let id = call.id.clone();

                async move {
                    let mut content = match handled {
                        Ok(handled) => match handled.await {
                            Ok(content) => content,
                            Err(e) => format!("Error: {e:#}"),
                        },
                        Err(e) => format!("Error: the arguments aren't valid JSON: {e}"),
                    };
                    if let Some(limit) = result_limit {
                        if content.len() > limit {
                            let mut end = limit;
                            while !content.is_char_boundary(end) {
                                end -= 1;
                            }
                            content.truncate(end);
                            content.push_str(
                                "\n[tool result truncated to stay within conversation limits]",
                            );
                        }
                    }
                    serde_json::json!({
                        "role": "tool",
                        "tool_call_id": id,
                        "content": content,
                    })
                }
            }))
            .await;
            self.body.messages.extend(results);
        }

        bail!(
            "{} didn't finish its turn within {} requests",
            self.body.model,
            self.max_requests
        )
    }

    /// Sends the request and reads the streamed response (server-sent events), handing its text
    /// to `sink` as it comes.
    async fn post(&self, sink: &mut TextSink<'_>) -> anyhow::Result<Response> {
        let mut token = match &self.bearer_source {
            Some(source) => Some(source.token(None).await?),
            None => None,
        };
        let mut retried = false;
        let mut resp = loop {
            let mut headers = self.headers.clone();
            if let Some(token) = &token {
                headers.insert(
                    AUTHORIZATION,
                    HeaderValue::from_str(&format!("Bearer {token}"))?,
                );
            }
            let resp = self
                .client
                .post(format!("{}/chat/completions", self.base_url))
                .headers(headers)
                .timeout(self.timeout)
                .json(&self.body)
                .send()
                .await
                .with_context(|| t!("provider.openai_unreachable", url = self.base_url))?;
            if resp.status() == StatusCode::UNAUTHORIZED && !retried {
                if let Some(source) = &self.bearer_source {
                    token = Some(source.token(token.as_deref()).await?);
                    retried = true;
                    continue;
                }
            }
            break resp;
        };
        if !resp.status().is_success() {
            return Err(ApiError::read(resp).await.into());
        }

        let mut response = Response::default();
        let mut line = Vec::new();
        while let Some(chunk) = resp
            .chunk()
            .await
            .context("The model's response stream failed")?
        {
            for &byte in chunk.iter() {
                if byte != b'\n' {
                    line.push(byte);
                    continue;
                }
                if response.line(&line, sink)? {
                    return response.finish();
                }
                line.clear();
            }
        }
        if response.line(&line, sink)? || response.finish_reason.is_some() {
            return response.finish();
        }

        bail!("The model's response stream ended before the message did")
    }
}

/// Undoes a turn's changes to the history unless it's kept: on an error, a refusal, or the
/// future being dropped midway.
struct Turn<'a> {
    conversation: &'a mut Conversation,
    start: usize,
    keep: bool,
    original: Option<Vec<serde_json::Value>>,
}

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        if !self.keep {
            if let Some(original) = self.original.take() {
                self.conversation.body.messages = original;
            } else {
                self.conversation.body.messages.truncate(self.start);
            }
        }
    }
}

/// A response put together from its stream.
#[derive(Default)]
struct Response {
    model: Option<String>,
    content: String,
    bono_content: Option<Vec<serde_json::Value>>,
    tool_calls: Vec<ToolCall>,
    finish_reason: Option<String>,
    usage: Usage,
}

/// A tool call, its pieces put together (by their index in the stream).
#[derive(Default)]
struct ToolCall {
    id: String,
    name: String,
    /// JSON text
    arguments: String,
}

impl Response {
    /// One line of the stream. Returns whether the stream is over (`data: [DONE]`).
    fn line(&mut self, line: &[u8], sink: &mut TextSink<'_>) -> anyhow::Result<bool> {
        #[derive(Deserialize)]
        struct Chunk {
            #[serde(default)]
            model: Option<String>,
            #[serde(default)]
            choices: Vec<Choice>,
            #[serde(default)]
            usage: Option<ChunkUsage>,
            #[serde(default)]
            error: Option<serde_json::Value>,
        }

        #[derive(Deserialize)]
        struct Choice {
            #[serde(default)]
            delta: Option<Delta>,
            #[serde(default)]
            finish_reason: Option<String>,
        }

        #[derive(Deserialize)]
        struct Delta {
            #[serde(default)]
            content: Option<String>,
            #[serde(default)]
            bono_content: Option<Vec<serde_json::Value>>,
            #[serde(default)]
            tool_calls: Vec<DeltaCall>,
        }

        #[derive(Deserialize)]
        struct DeltaCall {
            #[serde(default)]
            index: Option<usize>,
            #[serde(default)]
            id: Option<String>,
            #[serde(default)]
            function: Option<DeltaFunction>,
        }

        #[derive(Deserialize)]
        struct DeltaFunction {
            #[serde(default)]
            name: Option<String>,
            #[serde(default)]
            arguments: Option<String>,
        }

        #[derive(Deserialize)]
        struct ChunkUsage {
            #[serde(default)]
            prompt_tokens: u64,
            #[serde(default)]
            completion_tokens: u64,
            #[serde(default)]
            prompt_tokens_details: Option<PromptDetails>,
        }

        #[derive(Deserialize)]
        struct PromptDetails {
            #[serde(default)]
            cached_tokens: Option<u64>,
        }

        let line = line.strip_suffix(b"\r").unwrap_or(line);
        // Only the data matters: the events have no names, and comments keep the connection alive
        let Some(data) = line.strip_prefix(b"data:") else {
            return Ok(false);
        };
        let data = data.trim_ascii();
        if data == b"[DONE]" {
            return Ok(true);
        }
        if data.is_empty() {
            return Ok(false);
        }

        let chunk = serde_json::from_slice::<Chunk>(data)
            .context("The model sent a stream event that couldn't be read")?;
        if let Some(error) = chunk.error {
            let message = error
                .get("message")
                .and_then(serde_json::Value::as_str)
                .map_or_else(|| error.to_string(), str::to_owned);
            bail!("Model error: {message}");
        }
        if chunk.model.is_some() {
            self.model = chunk.model;
        }

        for choice in chunk.choices {
            if let Some(delta) = choice.delta {
                if let Some(blocks) = delta.bono_content {
                    if self.bono_content.is_some() {
                        bail!("Duplicate native history");
                    }
                    self.bono_content = Some(blocks);
                }
                if let Some(content) = delta.content {
                    sink.push(&content);
                    self.content.push_str(&content);
                }
                for piece in delta.tool_calls {
                    let index = piece
                        .index
                        .unwrap_or(self.tool_calls.len().saturating_sub(1));
                    if self.tool_calls.len() <= index {
                        self.tool_calls.resize_with(index + 1, ToolCall::default);
                    }
                    let call = &mut self.tool_calls[index];
                    if let Some(id) = piece.id {
                        call.id = id;
                    }
                    if let Some(function) = piece.function {
                        if let Some(name) = function.name {
                            call.name.push_str(&name);
                        }
                        if let Some(arguments) = function.arguments {
                            call.arguments.push_str(&arguments);
                        }
                    }
                }
            }
            if choice.finish_reason.is_some() {
                self.finish_reason = choice.finish_reason;
            }
        }

        if let Some(usage) = chunk.usage {
            let cached = usage
                .prompt_tokens_details
                .and_then(|details| details.cached_tokens)
                .unwrap_or(0);
            self.usage = Usage {
                input_tokens: usage.prompt_tokens.saturating_sub(cached),
                cache_write_tokens: 0,
                cache_read_tokens: cached,
                output_tokens: usage.completion_tokens,
            };
        }
        Ok(false)
    }

    fn finish(mut self) -> anyhow::Result<Self> {
        if self.finish_reason.is_none() {
            bail!("The model's response stream ended before the message did");
        }
        // A call needs an id for its result: some servers leave it out
        for (k, call) in self.tool_calls.iter_mut().enumerate() {
            if call.id.is_empty() {
                call.id = format!("call_{k}");
            }
        }
        self.tool_calls.retain(|call| !call.name.is_empty());
        Ok(self)
    }
}

/// An error the server answered with.
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: Option<String>,
    pub code: Option<String>,
}

impl ApiError {
    async fn read(resp: reqwest::Response) -> Self {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        let code = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|body| body["error"]["code"].as_str().map(str::to_owned));
        // `{"error": {"message": ...}}`, or `{"error": "..."}`
        let message = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|body| {
                let error = body.get("error")?;
                Some(
                    error
                        .get("message")
                        .and_then(serde_json::Value::as_str)
                        .or(error.as_str())?
                        .to_owned(),
                )
            })
            // Plain text, but not a web page (e.g. a website's 404 page): the status says enough
            .or_else(|| {
                Some(body).filter(|body| !body.trim().is_empty() && !body.trim().starts_with('<'))
            });
        Self {
            status,
            message,
            code,
        }
    }
}

impl Display for ApiError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "Model API error {}", self.status)?;
        if let Some(message) = &self.message {
            write!(f, ": {message}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ApiError {}

/// Optional budgets for providers with a small request envelope.
#[derive(Clone, Copy)]
pub struct HistoryLimits {
    pub max_bytes: usize,
    pub max_messages: usize,
    pub target: f64,
}

fn history_limit_error(error: &anyhow::Error) -> bool {
    error.downcast_ref::<ApiError>().is_some_and(|error| {
        error.status == StatusCode::PAYLOAD_TOO_LARGE
            || (error.status == StatusCode::BAD_REQUEST
                && error.code.as_deref() == Some("conversation_too_long"))
    })
}

fn strip_snapshot(text: &str) -> String {
    const BEGIN: &str = "[BEGIN UNTRUSTED TELEMETRY SNAPSHOT]";
    const END: &str = "[END UNTRUSTED TELEMETRY SNAPSHOT]";
    let mut text = text.to_owned();
    if let Some(start) = text.find(BEGIN) {
        if let Some(end) = text[start + BEGIN.len()..].find(END) {
            text.replace_range(
                start..start + BEGIN.len() + end + END.len(),
                "[telemetry snapshot omitted]",
            );
        }
    }
    text
}

pub struct ConversationBuilder {
    base_url: Option<String>,
    api_key: Option<String>,
    model: String,
    effort: String,
    system: Arc<str>,
    tools: Tools,
    max_tokens: NonZeroU32,
    max_requests: NonZeroU32,
    timeout: Duration,
    history_limits: Option<HistoryLimits>,
}

impl ConversationBuilder {
    pub fn new() -> Self {
        Self {
            base_url: None,
            api_key: None,
            model: String::new(),
            effort: String::new(),
            system: Arc::default(),
            tools: Tools::default(),
            max_tokens: NonZeroU32::new(16000).unwrap(),
            max_requests: NonZeroU32::new(16).unwrap(),
            timeout: Duration::from_mins(10),
            history_limits: None,
        }
    }

    /// The API's base, with its version, e.g. `http://127.0.0.1:8080/v1`. Defaults to
    /// [`DEFAULT_BASE_URL`].
    #[inline]
    pub fn base_url(&mut self, base_url: impl Into<String>) -> &mut Self {
        self.base_url = Some(base_url.into());
        self
    }

    /// Sent as a bearer token. Defaults to `OPENAI_API_KEY`, if set: local servers need none.
    #[inline]
    pub fn api_key(&mut self, api_key: impl Into<String>) -> &mut Self {
        self.api_key = Some(api_key.into());
        self
    }

    /// The model, as the server names it. A server serving one model (`llama-server`) takes any.
    #[inline]
    pub fn model(&mut self, model: impl Into<String>) -> &mut Self {
        self.model = model.into();
        self
    }

    /// `reasoning_effort`: `low`, `medium` or `high`, or `none` to not think at all (also turning
    /// thinking off in the chat template of llama.cpp's thinking models). Left out when empty: the
    /// model's default.
    #[inline]
    pub fn effort(&mut self, effort: impl Into<String>) -> &mut Self {
        self.effort = effort.into();
        self
    }

    /// The system prompt. Defaults to none.
    #[inline]
    pub fn system(&mut self, system: impl Into<Arc<str>>) -> &mut Self {
        self.system = system.into();
        self
    }

    /// A tool that runs on this side: see [`Tools::insert`].
    #[inline]
    #[allow(dead_code)]
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

    /// The most tokens in one response, thinking included. Defaults to 16000.
    #[inline]
    pub fn max_tokens(&mut self, max_tokens: NonZeroU32) -> &mut Self {
        self.max_tokens = max_tokens;
        self
    }

    pub fn history_limits(&mut self, limits: HistoryLimits) -> &mut Self {
        self.history_limits = Some(limits);
        self
    }

    pub fn build(self, client: reqwest::Client) -> anyhow::Result<Conversation> {
        if let Some(limits) = self.history_limits {
            anyhow::ensure!(
                limits.max_bytes > 0
                    && limits.max_messages > 0
                    && limits.target > 0.0
                    && limits.target < 1.0,
                "Invalid history limits"
            );
        }
        let base_url = base_url_or_default(self.base_url.as_deref());
        let headers = headers(self.api_key)?;

        let (reasoning_effort, chat_template_kwargs) = match self.effort.trim() {
            "" => (None, None),
            "none" => (
                Some("none".to_owned()),
                Some(serde_json::json!({"enable_thinking": false})),
            ),
            effort => (Some(effort.to_owned()), None),
        };

        let max_tokens = if is_openai(&base_url) {
            MaxTokens::MaxCompletionTokens(self.max_tokens)
        } else {
            MaxTokens::MaxTokens(self.max_tokens)
        };

        let mut messages = Vec::new();
        if !self.system.trim().is_empty() {
            messages.push(serde_json::json!({
                "role": "system",
                "content": &*self.system,
            }));
        }

        let tools = self
            .tools
            .definitions()
            .map(|tool| RequestTool {
                r#type: "function",
                function: RequestFunction {
                    name: tool.name,
                    description: tool.description.map(str::to_owned),
                    parameters: tool.input_schema.clone(),
                },
            })
            .collect();

        return Ok(Conversation {
            client,
            base_url,
            headers,
            bearer_source: None,
            native_history: false,
            history_limits: self.history_limits,
            kept: messages.len(),
            body: RequestBody {
                model: self.model,
                messages,
                tools,
                stream: true,
                stream_options: StreamOptions {
                    include_usage: true,
                },
                max_tokens,
                reasoning_effort,
                chat_template_kwargs,
            },
            timeout: self.timeout,
            max_requests: self.max_requests,
            tools: self.tools,
            usage: BTreeMap::new(),
        });
    }
}

fn base_url_or_default(base_url: Option<&str>) -> String {
    base_url
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .unwrap_or(DEFAULT_BASE_URL)
        .trim_end_matches('/')
        .to_owned()
}

/// Whether `base_url` is OpenAI's own API, whose reasoning models (o-series, GPT-5) take
/// `max_completion_tokens` and reject `max_tokens`, which compatible servers may only know.
fn is_openai(base_url: &str) -> bool {
    reqwest::Url::parse(base_url).is_ok_and(|url| url.host_str() == Some("api.openai.com"))
}

/// The bearer token: `api_key`, or else `OPENAI_API_KEY`, if set.
fn headers(api_key: Option<String>) -> anyhow::Result<HeaderMap> {
    let api_key = match api_key.filter(|key| !key.trim().is_empty()) {
        Some(api_key) => Some(api_key),
        None => match std::env::var("OPENAI_API_KEY") {
            Ok(api_key) if !api_key.trim().is_empty() => Some(api_key),
            Ok(_) | Err(VarError::NotPresent) => None,
            Err(VarError::NotUnicode(s)) => {
                bail!("environment variable was not valid unicode: {:?}", s)
            }
        },
    };

    let mut headers = HeaderMap::new();
    if let Some(api_key) = api_key {
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {api_key}"))?,
        );
    }
    Ok(headers)
}

#[derive(Serialize)]
struct RequestBody {
    model: String,
    /// Each message as it's sent: the model's own are kept as received
    messages: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<RequestTool>,
    /// Always: answers are spoken as they're written
    stream: bool,
    /// The tokens, in the stream's last event
    stream_options: StreamOptions,
    #[serde(flatten)]
    max_tokens: MaxTokens,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
    /// llama.cpp's: passed to the model's chat template
    #[serde(skip_serializing_if = "Option::is_none")]
    chat_template_kwargs: Option<serde_json::Value>,
}

/// The most tokens in one response, under the name the server knows.
#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum MaxTokens {
    MaxTokens(NonZeroU32),
    MaxCompletionTokens(NonZeroU32),
}

#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

#[derive(Serialize)]
struct RequestTool {
    r#type: &'static str,
    function: RequestFunction,
}

#[derive(Serialize)]
struct RequestFunction {
    name: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    parameters: serde_json::Value,
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

    /// Answers each request with the next of `responses` (status and body), and records each
    /// request's path and body. Past the last one, accepts connections and never answers.
    async fn mock(responses: Vec<(u16, String)>) -> (String, Arc<Mutex<Vec<(String, Value)>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
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
                let path = head.split(' ').nth(1).unwrap_or_default().to_owned();
                let length = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .map_or(0, |n| n.trim().parse::<usize>().unwrap());
                while buf.len() < body_start + length {
                    let n = stream.read(&mut chunk).await.unwrap();
                    buf.extend_from_slice(&chunk[..n]);
                }
                let body = serde_json::from_slice(&buf[body_start..]).unwrap_or(Value::Null);
                recorded.lock().unwrap().push((path, body));

                let Some((status, body)) = responses.next() else {
                    held.push(stream);
                    continue;
                };
                let response = format!(
                    "HTTP/1.1 {status} Mock\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });

        (url, requests)
    }

    /// A stream of `chunks` (each a `data:` event), ending with the usage and `[DONE]`.
    fn stream(chunks: Vec<Value>) -> (u16, String) {
        let mut events = chunks
            .iter()
            .map(|chunk| format!("data: {chunk}\n\n"))
            .collect::<String>();
        events.push_str(&format!(
            "data: {}\n\n",
            json!({"model": "qwen3.5-9b", "choices": [], "usage": {
                "prompt_tokens": 300, "completion_tokens": 20,
                "prompt_tokens_details": {"cached_tokens": 200}
            }})
        ));
        events.push_str("data: [DONE]\n\n");
        (200, events)
    }

    fn text(piece: &str) -> Value {
        json!({"model": "qwen3.5-9b", "choices": [{"index": 0, "delta": {"content": piece}, "finish_reason": null}]})
    }

    fn finish(reason: &str) -> Value {
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": reason}]})
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

    fn builder(url: &str, calls: &Arc<AtomicUsize>) -> ConversationBuilder {
        let mut builder = ConversationBuilder::new();
        builder
            .base_url(url)
            .api_key("test")
            .model("qwen3.5-9b")
            .system("You are a race engineer.")
            .tool(
                "lookup",
                Some("Looks things up".into()),
                json!({"type": "object", "properties": {"what": {"type": "string"}}}),
                Lookup(calls.clone()),
            );
        builder
    }

    async fn send_streaming(
        conversation: &mut Conversation,
        text: &str,
    ) -> anyhow::Result<(Reply, Vec<String>)> {
        let mut pieces = Vec::new();
        let reply = timeout(
            Duration::from_secs(5),
            conversation.send_streaming(text.into(), &mut |piece| pieces.push(piece.to_owned())),
        )
        .await
        .expect("timed out")?;
        Ok((reply, pieces))
    }

    #[test]
    fn trims_whole_turns_and_preserves_native_history() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut builder = builder("http://localhost", &calls);
        builder.history_limits(HistoryLimits {
            max_bytes: 100_000,
            max_messages: 12,
            target: 0.5,
        });
        let mut conversation = builder.build(reqwest::Client::new()).unwrap();
        for index in 0..4 {
            conversation.body.messages.extend([
                json!({"role": "user", "content": format!("Question {index}")}),
                json!({"role": "assistant", "tool_calls": [{"id": "call"}],
                    "bono_content": [{"type": "thinking", "signature": "signed"}]}),
                json!({"role": "tool", "tool_call_id": "call", "content": "result"}),
                json!({"role": "assistant", "content": "answer"}),
            ]);
        }
        let newest = conversation.body.messages[13..].to_vec();
        conversation.prepare_history("Next?", 0.5).unwrap();
        assert_eq!(conversation.body.messages[0]["role"], "system");
        assert_eq!(&conversation.body.messages[1..], newest);
        let stable = conversation.body.messages.clone();
        conversation.prepare_history("Next?", 0.5).unwrap();
        assert_eq!(conversation.body.messages, stable);
    }

    #[test]
    fn strips_only_stale_snapshot_sections_on_byte_trim() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut builder = builder("http://localhost", &calls);
        builder.history_limits(HistoryLimits {
            max_bytes: 10_000,
            max_messages: 100,
            target: 0.5,
        });
        let mut conversation = builder.build(reqwest::Client::new()).unwrap();
        for index in 0..6 {
            conversation.body.messages.push(json!({"role": "user", "content": format!(
                "Question {index} [BEGIN UNTRUSTED TELEMETRY SNAPSHOT]{}[END UNTRUSTED TELEMETRY SNAPSHOT] suffix",
                "x".repeat(if index < 2 { 6000 } else { 10 }))}));
            conversation
                .body
                .messages
                .push(json!({"role": "assistant", "content": "answer"}));
        }
        let recent = conversation.body.messages[5..].to_vec();
        conversation.prepare_history("Next?", 0.5).unwrap();
        assert_eq!(
            conversation.body.messages[1]["content"],
            "Question 0 [telemetry snapshot omitted] suffix"
        );
        assert_eq!(&conversation.body.messages[5..], recent);
    }

    #[tokio::test]
    async fn failed_recovery_restores_history_and_unrelated_errors_are_not_retried() {
        let limit = || (413, json!({"error": {"message": "too large"}}).to_string());
        let (url, requests) = mock(vec![
            limit(),
            limit(),
            limit(),
            (
                400,
                json!({"error": {"code": "invalid_request", "message": "bad"}}).to_string(),
            ),
        ])
        .await;
        let calls = Arc::new(AtomicUsize::new(0));
        let mut builder = builder(&url, &calls);
        builder.history_limits(HistoryLimits {
            max_bytes: 120_000,
            max_messages: 100,
            target: 0.5,
        });
        let mut conversation = builder.build(reqwest::Client::new()).unwrap();
        conversation.body.messages.extend([
            json!({"role": "user", "content": "Old question"}),
            json!({"role": "assistant", "content": "Old answer"}),
        ]);
        let original = conversation.body.messages.clone();
        assert!(send_streaming(&mut conversation, "Next?").await.is_err());
        assert_eq!(conversation.body.messages, original);
        assert_eq!(requests.lock().unwrap().len(), 3);
        assert!(send_streaming(&mut conversation, "Next?").await.is_err());
        assert_eq!(conversation.body.messages, original);
        assert_eq!(requests.lock().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn recovers_from_limit_errors_and_resets_as_a_last_resort() {
        let (url, requests) = mock(vec![
            (413, json!({"error": {"message": "too large"}}).to_string()),
            (
                400,
                json!({"error": {"code": "conversation_too_long", "message": "too long"}})
                    .to_string(),
            ),
            stream(vec![text("Answer"), finish("stop")]),
        ])
        .await;
        let calls = Arc::new(AtomicUsize::new(0));
        let mut builder = builder(&url, &calls);
        builder.history_limits(HistoryLimits {
            max_bytes: 120_000,
            max_messages: 100,
            target: 0.5,
        });
        let mut conversation = builder.build(reqwest::Client::new()).unwrap();
        conversation.body.messages.extend([
            json!({"role": "user", "content": "Old question"}),
            json!({"role": "assistant", "content": "Old answer"}),
        ]);
        assert_eq!(
            send_streaming(&mut conversation, "Next?")
                .await
                .unwrap()
                .0
                .text,
            "Answer"
        );
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[2].1["messages"].as_array().unwrap().len(), 2);
        assert_eq!(requests[2].1["messages"][1]["content"], "Next?");
    }

    #[tokio::test]
    async fn runs_tool_calls_put_together_from_the_stream() {
        let call =
            |delta: Value| json!({"choices": [{"index": 0, "delta": {"tool_calls": [delta]}}]});
        let (url, requests) = mock(vec![
            stream(vec![
                text("Let me "),
                text("check."),
                call(json!({"index": 0, "id": "call_1", "type": "function", "function": {"name": "lookup", "arguments": ""}})),
                call(json!({"index": 0, "function": {"arguments": "{\"what\": "}})),
                call(json!({"index": 0, "function": {"arguments": "\"gap\"}"}})),
                finish("tool_calls"),
            ]),
            stream(vec![text("\nThe gap "), text("is 1.2s."), finish("stop")]),
        ])
        .await;
        let calls = Arc::new(AtomicUsize::new(0));
        let mut conversation = builder(&url, &calls).build(reqwest::Client::new()).unwrap();

        let (reply, pieces) = send_streaming(&mut conversation, "Gap?").await.unwrap();
        assert_eq!(reply.stop, StopReason::EndTurn);
        assert_eq!(reply.text, "Let me check.\n\nThe gap is 1.2s.");
        assert_eq!(pieces.concat(), reply.text);
        assert_eq!(reply.model, "qwen3.5-9b");
        assert_eq!(
            reply.usage,
            Usage {
                input_tokens: 200,
                cache_write_tokens: 0,
                cache_read_tokens: 400,
                output_tokens: 40,
            }
        );
        assert_eq!(calls.load(Ordering::Relaxed), 1);

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let (path, first) = &requests[0];
        assert_eq!(path, "/v1/chat/completions");
        assert_eq!(first["stream"], true);
        assert_eq!(first["stream_options"]["include_usage"], true);
        assert_eq!(first["model"], "qwen3.5-9b");
        assert!(first.get("reasoning_effort").is_none());
        assert_eq!(first["tools"][0]["function"]["name"], "lookup");
        assert_eq!(first["messages"][0]["role"], "system");

        let messages = requests[1].1["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[2]["role"], "assistant");
        assert_eq!(messages[2]["content"], "Let me check.");
        assert_eq!(messages[2]["tool_calls"][0]["id"], "call_1");
        assert_eq!(
            messages[2]["tool_calls"][0]["function"]["arguments"],
            "{\"what\": \"gap\"}"
        );
        assert_eq!(messages[3]["role"], "tool");
        assert_eq!(messages[3]["tool_call_id"], "call_1");
        assert_eq!(messages[3]["content"], "looked up \"gap\"");
        drop(requests);

        assert_eq!(conversation.body.messages.len(), 5);
        conversation.reset();
        assert_eq!(conversation.body.messages.len(), 1);
    }

    #[tokio::test]
    async fn preserves_native_thinking_silently_across_tool_rounds() {
        let call =
            |delta: Value| json!({"choices": [{"index": 0, "delta": {"tool_calls": [delta]}}]});
        let native = json!([
            {"type":"thinking", "thinking":"private summary", "signature":"signed-block"},
            {"type":"redacted_thinking", "data":"opaque-data"},
            {"type":"text", "text":"Let me check."},
            {"type":"tool_use", "id":"call_1", "name":"lookup", "input":{"what":"gap"}}
        ]);
        let (url, requests) = mock(vec![
            stream(vec![
                text("Let me "),
                text("check."),
                call(json!({"index": 0, "id": "call_1", "type": "function", "function": {"name": "lookup", "arguments": ""}})),
                call(json!({"index": 0, "function": {"arguments": "{\"what\": "}})),
                call(json!({"index": 0, "function": {"arguments": "\"gap\"}"}})),
                json!({"choices":[{"delta":{"bono_content":native.clone()}}]}),
                finish("tool_calls"),
            ]),
            stream(vec![text("\nThe gap "), text("is 1.2s."), finish("stop")]),
        ])
        .await;
        let calls = Arc::new(AtomicUsize::new(0));
        let mut conversation = builder(&url, &calls).build(reqwest::Client::new()).unwrap();

        conversation.enable_native_history();
        let (reply, pieces) = send_streaming(&mut conversation, "Gap?").await.unwrap();
        assert_eq!(reply.stop, StopReason::EndTurn);
        assert_eq!(reply.text, "Let me check.\n\nThe gap is 1.2s.");
        assert_eq!(pieces.concat(), reply.text);
        assert_eq!(reply.model, "qwen3.5-9b");
        assert_eq!(
            reply.usage,
            Usage {
                input_tokens: 200,
                cache_write_tokens: 0,
                cache_read_tokens: 400,
                output_tokens: 40,
            }
        );
        assert_eq!(calls.load(Ordering::Relaxed), 1);

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let (path, first) = &requests[0];
        assert_eq!(path, "/v1/chat/completions");
        assert!(first.get("bono_native_history").is_none());
        assert_eq!(first["stream"], true);
        assert_eq!(first["stream_options"]["include_usage"], true);
        assert_eq!(first["model"], "qwen3.5-9b");
        assert!(first.get("reasoning_effort").is_none());
        assert_eq!(first["tools"][0]["function"]["name"], "lookup");
        assert_eq!(first["messages"][0]["role"], "system");

        let messages = requests[1].1["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[2]["bono_content"], native);
        assert_eq!(messages[2]["role"], "assistant");
        assert_eq!(messages[2]["content"], "Let me check.");
        assert_eq!(messages[2]["tool_calls"][0]["id"], "call_1");
        assert_eq!(
            messages[2]["tool_calls"][0]["function"]["arguments"],
            "{\"what\": \"gap\"}"
        );
        assert_eq!(messages[3]["role"], "tool");
        assert_eq!(messages[3]["tool_call_id"], "call_1");
        assert_eq!(messages[3]["content"], "looked up \"gap\"");
        drop(requests);

        assert_eq!(conversation.body.messages.len(), 5);
        conversation.reset();
        assert_eq!(conversation.body.messages.len(), 1);
    }

    #[tokio::test]
    async fn turns_thinking_off_when_asked() {
        let (url, requests) = mock(vec![stream(vec![text("Box."), finish("stop")])]).await;
        let calls = Arc::new(AtomicUsize::new(0));
        let mut builder = builder(&url, &calls);
        builder.effort("none");
        let mut conversation = builder.build(reqwest::Client::new()).unwrap();
        send_streaming(&mut conversation, "Pit?").await.unwrap();

        let requests = requests.lock().unwrap();
        assert_eq!(requests[0].1["reasoning_effort"], "none");
        assert_eq!(
            requests[0].1["chat_template_kwargs"]["enable_thinking"],
            false
        );
    }

    #[test]
    fn limits_tokens_as_the_server_knows() {
        let body = |url: &str| {
            let mut builder = ConversationBuilder::new();
            builder
                .base_url(url)
                .api_key("test")
                .max_tokens(NonZeroU32::new(1024).unwrap());
            let conversation = builder.build(reqwest::Client::new()).unwrap();
            serde_json::to_value(&conversation.body).unwrap()
        };

        let openai = body("https://api.openai.com/v1/");
        assert_eq!(openai["max_completion_tokens"], 1024);
        assert!(openai.get("max_tokens").is_none());

        for url in [
            "http://127.0.0.1:8080/v1",
            "http://localhost:1234/v1",
            "https://openrouter.ai/api/v1",
        ] {
            let compatible = body(url);
            assert_eq!(compatible["max_tokens"], 1024, "{url}");
            assert!(compatible.get("max_completion_tokens").is_none(), "{url}");
        }
    }

    #[tokio::test]
    async fn ends_turns_as_the_model_does() {
        let (url, _) = mock(vec![
            stream(vec![text("Box, box"), finish("length")]),
            stream(vec![text("No."), finish("content_filter")]),
        ])
        .await;
        let calls = Arc::new(AtomicUsize::new(0));
        let mut conversation = builder(&url, &calls).build(reqwest::Client::new()).unwrap();

        let (reply, _) = send_streaming(&mut conversation, "Pit?").await.unwrap();
        assert_eq!(reply.stop, StopReason::MaxTokens);
        assert_eq!(conversation.body.messages.len(), 3);

        // A refusal isn't kept
        let (reply, _) = send_streaming(&mut conversation, "Pit?").await.unwrap();
        assert_eq!(reply.stop, StopReason::Refusal);
        assert_eq!(conversation.body.messages.len(), 3);
    }

    #[tokio::test]
    async fn reports_errors_and_forgets_the_exchange() {
        let (url, _) = mock(vec![
            (
                400,
                json!({"error": {"message": "the request exceeds the available context size", "type": "exceed_context_size_error"}}).to_string(),
            ),
            (
                200,
                format!("data: {}\n\ndata: {}\n\n", text("Bo"), json!({"error": {"message": "out of memory"}})),
            ),
            (200, format!("data: {}\n\n", text("Bo"))),
        ])
        .await;
        let calls = Arc::new(AtomicUsize::new(0));
        let mut conversation = builder(&url, &calls).build(reqwest::Client::new()).unwrap();

        let error = send_streaming(&mut conversation, "Pit?").await.unwrap_err();
        let error = error.downcast_ref::<ApiError>().unwrap();
        assert_eq!(error.status, StatusCode::BAD_REQUEST);
        assert_eq!(
            error.message.as_deref(),
            Some("the request exceeds the available context size")
        );
        assert_eq!(conversation.body.messages.len(), 1);

        let error = send_streaming(&mut conversation, "Pit?").await.unwrap_err();
        assert!(format!("{error:#}").contains("out of memory"), "{error:#}");
        assert_eq!(conversation.body.messages.len(), 1);

        let error = send_streaming(&mut conversation, "Pit?").await.unwrap_err();
        assert!(format!("{error:#}").contains("ended before"), "{error:#}");
        assert_eq!(conversation.body.messages.len(), 1);
    }

    #[tokio::test]
    async fn lists_the_models() {
        let (url, requests) = mock(vec![(
            200,
            json!({"object": "list", "data": [{"id": "qwen3.5-9b"}, {"id": "gemma-4"}]})
                .to_string(),
        )])
        .await;
        let models = models(Some(&url), Some("test"), &reqwest::Client::new())
            .await
            .unwrap();
        assert_eq!(
            models.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
            ["gemma-4", "qwen3.5-9b"]
        );
        assert_eq!(requests.lock().unwrap()[0].0, "/v1/models");
    }

    /// Against a server on this PC (`OPENAI_BASE_URL`, default llama.cpp's), with its model
    /// (`OPENAI_MODEL`).
    #[tokio::test]
    #[ignore]
    async fn answers_from_a_local_server() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut builder = ConversationBuilder::new();
        if let Ok(url) = std::env::var("OPENAI_BASE_URL") {
            builder.base_url(url);
        }
        builder
            .model(std::env::var("OPENAI_MODEL").unwrap_or_default())
            .effort("none")
            .system("You are a race engineer. Use the lookup tool for anything about the car.")
            .tool(
                "lookup",
                Some("Looks up the car's telemetry: `what` is e.g. `gap`".into()),
                json!({"type": "object", "properties": {"what": {"type": "string"}}, "required": ["what"]}),
                Lookup(calls.clone()),
            );
        let mut conversation = builder.build(reqwest::Client::new()).unwrap();
        let (reply, pieces) =
            send_streaming(&mut conversation, "Look up the gap, then tell me it.")
                .await
                .unwrap();
        println!("{reply:#?}\n{pieces:?}");
        assert_eq!(reply.stop, StopReason::EndTurn);
        assert!(calls.load(Ordering::Relaxed) > 0);
    }
}
