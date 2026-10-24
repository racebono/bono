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

/// A conversation with a model behind an OpenAI-compatible API.
pub struct Conversation {
    client: reqwest::Client,
    base_url: String,
    headers: HeaderMap,
    body: RequestBody,
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
            let results = join_all(calls.iter().map(|call| {
                // The arguments are JSON text, empty for none
                let input = match call.arguments.trim() {
                    "" => Ok(serde_json::json!({})),
                    json => serde_json::from_str::<serde_json::Value>(json),
                };
                let handled = input.map(|input| tools.handle(&call.name, &input));
                let id = call.id.clone();

                async move {
                    let content = match handled {
                        Ok(handled) => match handled.await {
                            Ok(content) => content,
                            Err(e) => format!("Error: {e:#}"),
                        },
                        Err(e) => format!("Error: the arguments aren't valid JSON: {e}"),
                    };
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
        let mut resp = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .headers(self.headers.clone())
            .timeout(self.timeout)
            .json(&self.body)
            .send()
            .await
            .with_context(|| t!("provider.openai_unreachable", url = self.base_url))?;
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
}

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        if !self.keep {
            self.conversation.body.messages.truncate(self.start);
        }
    }
}

/// A response put together from its stream.
#[derive(Default)]
struct Response {
    model: Option<String>,
    content: String,
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
                if let Some(content) = delta.content {
                    sink.push(&content);
                    self.content.push_str(&content);
                }
                for piece in delta.tool_calls {
                    let index = piece.index.unwrap_or_else(|| {
                        let new_call = self.tool_calls.last().is_some_and(|call| {
                            piece
                                .id
                                .as_ref()
                                .is_some_and(|id| !call.id.is_empty() && *id != call.id)
                                || (piece.function.as_ref().is_some_and(|f| f.name.is_some())
                                    && !call.name.is_empty())
                        });
                        if new_call {
                            self.tool_calls.len()
                        } else {
                            self.tool_calls.len().saturating_sub(1)
                        }
                    });
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
}

impl ApiError {
    async fn read(resp: reqwest::Response) -> Self {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
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
        Self { status, message }
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

    pub fn build(self, client: reqwest::Client) -> anyhow::Result<Conversation> {
        let base_url = base_url_or_default(self.base_url.as_deref());
        let headers = headers(self.api_key)?;

        let (reasoning_effort, chat_template_kwargs) = match self.effort.trim() {
            "" => (None, None),
            "none" if !is_openai(&base_url) => (
                Some("none".to_owned()),
                (!is_openai(&base_url)).then(|| serde_json::json!({"enable_thinking": false})),
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
    fn assembles_indexless_parallel_tool_calls_and_continuations() {
        for second in [
            json!({"id": "call_2", "function": {"name": "lookup", "arguments": "{\"b\":2}"}}),
            json!({"id": "call_2", "function": {"arguments": "{\"b\":2}"}}),
            json!({"function": {"name": "lookup", "arguments": "{\"b\":2}"}}),
        ] {
            let mut response = Response::default();
            let mut on_text = |_: &str| {};
            let mut sink = TextSink::new(&mut on_text);
            for piece in [
                json!({"id": "call_1", "function": {"name": "lookup", "arguments": "{\"a\":"}}),
                json!({"id": "call_1", "function": {"arguments": "1"}}),
                json!({"function": {"arguments": "}"}}),
                second.clone(),
            ] {
                let line = format!(
                    "data: {}",
                    json!({"choices": [{"delta": {"tool_calls": [piece]}}]})
                );
                response.line(line.as_bytes(), &mut sink).unwrap();
            }
            assert_eq!(response.tool_calls.len(), 2);
            assert_eq!(response.tool_calls[0].id, "call_1");
            assert_eq!(response.tool_calls[0].name, "lookup");
            assert_eq!(response.tool_calls[0].arguments, "{\"a\":1}");
            assert_eq!(
                response.tool_calls[1].id,
                second["id"].as_str().unwrap_or("")
            );
            assert_eq!(
                response.tool_calls[1].name,
                second["function"]["name"].as_str().unwrap_or("")
            );
            assert_eq!(response.tool_calls[1].arguments, "{\"b\":2}");
        }
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
    fn omits_chat_template_kwargs_for_openai() {
        let mut builder = ConversationBuilder::new();
        builder
            .base_url("https://api.openai.com/v1/")
            .api_key("test")
            .effort("none");
        let conversation = builder.build(reqwest::Client::new()).unwrap();
        let body = serde_json::to_value(&conversation.body).unwrap();

        assert_eq!(body["reasoning_effort"], "none");
        assert!(body.get("chat_template_kwargs").is_none());
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
