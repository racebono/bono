//! Models run by Ollama (<https://ollama.com>), on this PC or another, through its chat API.

use super::{Model, Reply, StopReason, ToolHandler, Tools, Usage, text::TextSink};
use crate::config::OllamaConfig;
use anyhow::{Context, bail};
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

/// Where Ollama listens unless `OLLAMA_HOST` says otherwise.
const DEFAULT_HOST: &str = "http://127.0.0.1:11434";

/// A conversation with a model on Ollama.
pub struct Conversation {
    client: reqwest::Client,
    base_url: String,
    headers: HeaderMap,
    body: RequestBody,
    /// The messages kept through a reset: the system prompt's
    kept: usize,
    /// How much it thinks, if the model can: whether it can is asked once, before the first turn
    think: Option<serde_json::Value>,
    can_think: Option<bool>,
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
            self.check_thinking().await?;

            let mut turn = Turn {
                start: self.body.messages.len(),
                conversation: self,
                keep: false,
            };
            let reply = turn
                .conversation
                .run_turn(text, &mut TextSink::new(on_text))
                .await?;

            turn.keep = true;
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
    /// A conversation with a model on Ollama as `config` says, with the defaults for the rest.
    pub fn new(
        config: &OllamaConfig,
        system: Arc<str>,
        tools: Tools,
        client: reqwest::Client,
    ) -> anyhow::Result<Self> {
        let mut builder = ConversationBuilder::new();
        if let Some(host) = &config.host {
            builder.base_url(host_url(host));
        }
        if let Some(keep_alive) = config.keep_alive {
            builder.keep_alive(keep_alive);
        }
        builder
            .model(&config.model)
            .effort(&config.effort)
            .context_length(config.context_length)
            .max_tokens(config.max_tokens)
            .system(system)
            .tools(tools);
        builder.build(client)
    }

    /// Asks Ollama, once, whether the model can think: asking one that can't to think is an
    /// error, so it's only asked to when it can. Also where a model that isn't there (not pulled)
    /// is found out.
    async fn check_thinking(&mut self) -> anyhow::Result<()> {
        if self.think.is_none() || self.can_think.is_some() {
            return Ok(());
        }

        #[derive(Serialize)]
        struct ShowRequest<'a> {
            model: &'a str,
        }

        #[derive(Deserialize)]
        struct ShowResponse {
            #[serde(default)]
            capabilities: Vec<String>,
        }

        let resp = self
            .client
            .post(format!("{}/api/show", self.base_url))
            .headers(self.headers.clone())
            .timeout(self.timeout)
            .json(&ShowRequest {
                model: &self.body.model,
            })
            .send()
            .await
            .with_context(|| self.unreachable())?;
        if !resp.status().is_success() {
            return Err(ApiError::read(resp).await.into());
        }

        let show = resp
            .json::<ShowResponse>()
            .await
            .context("Ollama sent a model description that couldn't be read")?;
        let can_think = show.capabilities.iter().any(|c| c == "thinking");
        if !can_think {
            log::info!("{} can't think: it answers straight away", self.body.model);
        }
        self.can_think = Some(can_think);
        Ok(())
    }

    async fn run_turn(&mut self, text: String, sink: &mut TextSink<'_>) -> anyhow::Result<Reply> {
        self.body.messages.push(serde_json::json!({
            "role": "user",
            "content": text,
        }));
        self.body.think = match self.can_think {
            Some(true) => self.think.clone(),
            _ => None,
        };

        let mut turn_usage = Usage::default();
        for _ in 0..self.max_requests.get() {
            sink.start_response();
            let response = self.post(sink).await?;
            sink.end_block();

            let model = response.model.unwrap_or_else(|| self.body.model.clone());
            let cached = response.prompt_eval_cached_count;
            let usage = Usage {
                input_tokens: response.prompt_eval_count.saturating_sub(cached),
                cache_write_tokens: 0,
                cache_read_tokens: cached,
                output_tokens: response.eval_count,
            };
            turn_usage += usage;
            *self.usage.entry(model.clone()).or_default() += usage;

            let calls = response.tool_calls;
            let stop = match response.done_reason.as_deref() {
                Some("stop") if !calls.is_empty() => None,
                Some("stop") => Some(StopReason::EndTurn),
                // Ollama cuts the prompt to fit the context window without saying so: running out
                // of room is all there is to go by
                Some("length")
                    if response.prompt_eval_count + response.eval_count
                        >= u64::from(self.body.options.num_ctx.get()) =>
                {
                    Some(StopReason::ContextWindowExceeded)
                }
                Some("length") => Some(StopReason::MaxTokens),
                other => bail!("Unexpected stop reason from Ollama: {other:?}"),
            };

            // Except when it ended with tool calls that won't run (cut off): calls without their
            // results would confuse the model later on
            let said = !response.content.is_empty() || !calls.is_empty();
            if said && (stop.is_none() || calls.is_empty()) {
                let mut message = serde_json::json!({
                    "role": "assistant",
                    "content": response.content,
                });
                // Sent back as received
                if !response.thinking.is_empty() {
                    message["thinking"] = response.thinking.into();
                }
                if !calls.is_empty() {
                    message["tool_calls"] = calls.clone().into();
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
                let function = &call["function"];
                let name = function["name"].as_str().unwrap_or_default().to_owned();
                // Usually an object; some models write it as JSON text
                let input = match &function["arguments"] {
                    serde_json::Value::String(json) => {
                        serde_json::from_str(json).unwrap_or(serde_json::Value::Null)
                    }
                    arguments => arguments.clone(),
                };
                let handled = tools.handle(&name, &input);
                let id = call.get("id").cloned();

                async move {
                    let content = match handled.await {
                        Ok(content) => content,
                        Err(e) => format!("Error: {e:#}"),
                    };
                    let mut result = serde_json::json!({
                        "role": "tool",
                        "tool_name": name,
                        "content": content,
                    });
                    if let Some(id) = id {
                        result["tool_call_id"] = id;
                    }
                    result
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

    /// Sends the request and reads the streamed response, handing its text to `sink` as it
    /// comes.
    async fn post(&self, sink: &mut TextSink<'_>) -> anyhow::Result<Response> {
        let mut resp = self
            .client
            .post(format!("{}/api/chat", self.base_url))
            .headers(self.headers.clone())
            .timeout(self.timeout)
            .json(&self.body)
            .send()
            .await
            .with_context(|| self.unreachable())?;
        if !resp.status().is_success() {
            return Err(ApiError::read(resp).await.into());
        }

        // One JSON object per line
        let mut response = Response::default();
        let mut line = Vec::new();
        while let Some(chunk) = resp
            .chunk()
            .await
            .context("Ollama's response stream failed")?
        {
            for &byte in chunk.iter() {
                if byte != b'\n' {
                    line.push(byte);
                    continue;
                }
                if response.chunk(&line, sink)? {
                    return Ok(response);
                }
                line.clear();
            }
        }
        if response.chunk(&line, sink)? {
            return Ok(response);
        }

        bail!("Ollama's response stream ended before the message did")
    }

    fn unreachable(&self) -> String {
        format!(
            "Couldn't reach Ollama at {} (is it running?)",
            self.base_url
        )
    }
}

/// Undoes a turn's changes to the history unless it's kept: on an error, or the future being
/// dropped midway.
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
    thinking: String,
    tool_calls: Vec<serde_json::Value>,
    done_reason: Option<String>,
    /// Every token of the prompt, those read from the cache included
    prompt_eval_count: u64,
    prompt_eval_cached_count: u64,
    eval_count: u64,
}

impl Response {
    /// One line of the stream. Returns whether the response is complete.
    fn chunk(&mut self, line: &[u8], sink: &mut TextSink<'_>) -> anyhow::Result<bool> {
        #[derive(Deserialize)]
        struct Chunk {
            #[serde(default)]
            model: Option<String>,
            #[serde(default)]
            message: Option<ChunkMessage>,
            #[serde(default)]
            done: bool,
            #[serde(default)]
            done_reason: Option<String>,
            #[serde(default)]
            prompt_eval_count: u64,
            #[serde(default)]
            prompt_eval_cached_count: u64,
            #[serde(default)]
            eval_count: u64,
            #[serde(default)]
            error: Option<String>,
        }

        #[derive(Deserialize)]
        struct ChunkMessage {
            #[serde(default)]
            content: String,
            #[serde(default)]
            thinking: String,
            #[serde(default)]
            tool_calls: Vec<serde_json::Value>,
        }

        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.iter().all(u8::is_ascii_whitespace) {
            return Ok(false);
        }

        let chunk = serde_json::from_slice::<Chunk>(line)
            .context("Ollama sent a stream chunk that couldn't be read")?;
        if let Some(error) = chunk.error {
            bail!("Ollama error: {error}");
        }

        if let Some(message) = chunk.message {
            sink.push(&message.content);
            self.content.push_str(&message.content);
            self.thinking.push_str(&message.thinking);
            self.tool_calls.extend(message.tool_calls);
        }
        if chunk.model.is_some() {
            self.model = chunk.model;
        }
        if chunk.done {
            self.done_reason = chunk.done_reason;
            self.prompt_eval_count = chunk.prompt_eval_count;
            self.prompt_eval_cached_count = chunk.prompt_eval_cached_count;
            self.eval_count = chunk.eval_count;
        }
        Ok(chunk.done)
    }
}

/// An error Ollama answered with.
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: Option<String>,
}

impl ApiError {
    async fn read(resp: reqwest::Response) -> Self {
        #[derive(Deserialize)]
        struct Body {
            error: String,
        }

        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        let message = match serde_json::from_str::<Body>(&body) {
            Ok(body) => Some(body.error),
            Err(_) => Some(body).filter(|body| !body.trim().is_empty()),
        };
        Self { status, message }
    }
}

impl Display for ApiError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "Ollama error {}", self.status)?;
        if let Some(message) = &self.message {
            write!(f, ": {message}")?;
        }
        if self.status == StatusCode::NOT_FOUND {
            f.write_str(" (pull it with `ollama pull`)")?;
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
    context_length: NonZeroU32,
    max_tokens: NonZeroU32,
    keep_alive: Option<f64>,
    max_requests: NonZeroU32,
    timeout: Duration,
}

impl ConversationBuilder {
    pub fn new() -> Self {
        Self {
            base_url: None,
            api_key: None,
            model: "qwen3.5:9b".into(),
            effort: String::new(),
            system: Arc::default(),
            tools: Tools::default(),
            context_length: NonZeroU32::new(32768).unwrap(),
            max_tokens: NonZeroU32::new(16000).unwrap(),
            keep_alive: None,
            max_requests: NonZeroU32::new(16).unwrap(),
            timeout: Duration::from_mins(10),
        }
    }

    /// Defaults to `OLLAMA_HOST` (as the `ollama` command reads it), then
    /// `http://127.0.0.1:11434`.
    #[inline]
    pub fn base_url(&mut self, base_url: impl Into<String>) -> &mut Self {
        self.base_url = Some(base_url.into());
        self
    }

    /// Sent as a bearer token, for Ollama's cloud (`https://ollama.com`). Defaults to
    /// `OLLAMA_API_KEY`, if set: a local Ollama needs none.
    #[inline]
    pub fn api_key(&mut self, api_key: impl Into<String>) -> &mut Self {
        self.api_key = Some(api_key.into());
        self
    }

    /// Defaults to `qwen3.5:9b`.
    #[inline]
    pub fn model(&mut self, model: impl Into<String>) -> &mut Self {
        self.model = model.into();
        self
    }

    /// How much it thinks, on models that can: `low`, `medium` or `high` (`xhigh` and `max` are
    /// `high`), or `none`. Defaults to the model's own default.
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

    /// The context window the model is loaded with, in tokens: Ollama silently drops the start
    /// of a conversation that doesn't fit. More takes more memory. Defaults to 32768.
    #[inline]
    pub fn context_length(&mut self, context_length: NonZeroU32) -> &mut Self {
        self.context_length = context_length;
        self
    }

    /// The most tokens the model writes in one response, thinking included. Defaults to 16000.
    #[inline]
    pub fn max_tokens(&mut self, max_tokens: NonZeroU32) -> &mut Self {
        self.max_tokens = max_tokens;
        self
    }

    /// How long Ollama keeps the model loaded after a request, in seconds (negative: until it
    /// stops). Defaults to Ollama's own (5 minutes, unless `OLLAMA_KEEP_ALIVE` says otherwise).
    #[inline]
    pub fn keep_alive(&mut self, keep_alive: f64) -> &mut Self {
        self.keep_alive = Some(keep_alive);
        self
    }

    /// The most requests one `send` makes (one per round of tool calls). Defaults to 16.
    #[inline]
    pub fn max_requests(&mut self, max_requests: NonZeroU32) -> &mut Self {
        self.max_requests = max_requests;
        self
    }

    /// How long one request may take, loading the model included. Defaults to 10 minutes.
    #[inline]
    pub fn timeout(&mut self, timeout: Duration) -> &mut Self {
        self.timeout = timeout;
        self
    }

    pub fn build(self, client: reqwest::Client) -> anyhow::Result<Conversation> {
        let base_url = match self.base_url {
            Some(base_url) => base_url.trim_end_matches('/').to_owned(),
            None => match env("OLLAMA_HOST")? {
                Some(host) => host_url(&host),
                None => DEFAULT_HOST.to_owned(),
            },
        };

        let mut headers = HeaderMap::new();
        let api_key = match self.api_key {
            Some(api_key) => Some(api_key),
            None => env("OLLAMA_API_KEY")?,
        };
        if let Some(api_key) = api_key {
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {api_key}"))?,
            );
        }

        let think = match self.effort.trim() {
            "" => None,
            "none" | "off" | "false" => Some(false.into()),
            "xhigh" | "max" => Some("high".into()),
            effort => Some(effort.into()),
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
                think: None,
                options: RequestOptions {
                    num_ctx: self.context_length,
                    num_predict: self.max_tokens,
                },
                keep_alive: self.keep_alive,
            },
            think,
            can_think: None,
            timeout: self.timeout,
            max_requests: self.max_requests,
            tools: self.tools,
            usage: BTreeMap::new(),
        });
    }
}

fn env(name: &str) -> anyhow::Result<Option<String>> {
    match std::env::var(name) {
        Ok(value) if !value.trim().is_empty() => Ok(Some(value)),
        Ok(_) | Err(VarError::NotPresent) => Ok(None),
        Err(VarError::NotUnicode(s)) => {
            bail!("environment variable was not valid unicode: {:?}", s)
        }
    }
}

/// `OLLAMA_HOST` as the `ollama` command reads it: a host, with or without a scheme or port
/// (`0.0.0.0`, where the server listens on every address, is reached on this PC).
fn host_url(host: &str) -> String {
    let host = host.trim().trim_end_matches('/');
    let (scheme, rest) = match host.split_once("://") {
        Some((scheme, rest)) => (scheme, rest),
        None => ("http", host),
    };
    let (host, path) = match rest.split_once('/') {
        Some((host, path)) => (host, format!("/{path}")),
        None => (rest, String::new()),
    };

    // An IPv6 address's colons aren't a port's
    let has_port = match host.rfind(']') {
        Some(end) => host[end..].contains(':'),
        None => host.contains(':'),
    };
    let host = match host {
        "" | "0.0.0.0" => "127.0.0.1",
        host => host,
    };
    let port = match (has_port, scheme) {
        (true, _) => "",
        (false, "https") => ":443",
        (false, _) => ":11434",
    };
    format!("{scheme}://{host}{port}{path}")
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
    #[serde(skip_serializing_if = "Option::is_none")]
    think: Option<serde_json::Value>,
    options: RequestOptions,
    /// Seconds
    #[serde(skip_serializing_if = "Option::is_none")]
    keep_alive: Option<f64>,
}

#[derive(Serialize)]
struct RequestOptions {
    num_ctx: NonZeroU32,
    num_predict: NonZeroU32,
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

    /// Answers each request with the next of `responses` (status and body: lines of JSON are a
    /// stream), and records each request's path and body. Past the last one, accepts connections
    /// and never answers.
    async fn mock(responses: Vec<(u16, String)>) -> (String, Arc<Mutex<Vec<(String, Value)>>>) {
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
                let path = head.split(' ').nth(1).unwrap_or_default().to_owned();
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
                    .push((path, serde_json::from_slice(&buf[body_start..]).unwrap()));

                let Some((status, body)) = responses.next() else {
                    held.push(stream);
                    continue;
                };
                let response = format!(
                    "HTTP/1.1 {status} Mock\r\ncontent-type: application/x-ndjson\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });

        (url, requests)
    }

    /// A streamed response: `pieces` of text, then `calls`, then the end.
    fn stream(pieces: &[&str], calls: Value, done_reason: &str) -> (u16, String) {
        let mut lines = pieces
            .iter()
            .map(|piece| {
                json!({"model": "qwen3.5:9b", "message": {"role": "assistant", "content": piece}, "done": false})
            })
            .collect::<Vec<_>>();
        if calls != Value::Null {
            lines.push(json!({"model": "qwen3.5:9b", "message": {"role": "assistant", "content": "", "tool_calls": calls}, "done": false}));
        }
        lines.push(json!({
            "model": "qwen3.5:9b",
            "message": {"role": "assistant", "content": ""},
            "done": true,
            "done_reason": done_reason,
            "prompt_eval_count": 300,
            "prompt_eval_cached_count": 200,
            "eval_count": 20,
        }));
        let lines = lines.iter().map(Value::to_string).collect::<Vec<_>>();
        (200, lines.join("\n") + "\n")
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

    #[tokio::test]
    async fn runs_tool_calls_and_streams_the_text() {
        let (url, requests) = mock(vec![
            stream(
                &["Let me ", "check."],
                json!([{"id": "call_1", "function": {"index": 0, "name": "lookup", "arguments": {"what": "gap"}}}]),
                "stop",
            ),
            stream(&["\n", "The gap ", "is 1.2s."], Value::Null, "stop"),
        ])
        .await;
        let calls = Arc::new(AtomicUsize::new(0));
        let mut conversation = builder(&url, &calls).build(reqwest::Client::new()).unwrap();

        let (reply, pieces) = send_streaming(&mut conversation, "Gap?").await.unwrap();
        assert_eq!(reply.stop, StopReason::EndTurn);
        assert_eq!(reply.text, "Let me check.\n\nThe gap is 1.2s.");
        assert_eq!(pieces.concat(), reply.text);
        assert_eq!(reply.model, "qwen3.5:9b");
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
        assert_eq!(path, "/api/chat");
        assert_eq!(first["stream"], true);
        assert!(first.get("think").is_none());
        assert_eq!(first["tools"][0]["type"], "function");
        assert_eq!(first["tools"][0]["function"]["name"], "lookup");
        assert_eq!(first["messages"][0]["role"], "system");
        assert_eq!(first["options"]["num_ctx"], 32768);

        let messages = requests[1].1["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[2]["role"], "assistant");
        assert_eq!(messages[2]["content"], "Let me check.");
        assert_eq!(messages[2]["tool_calls"][0]["id"], "call_1");
        assert_eq!(messages[3]["role"], "tool");
        assert_eq!(messages[3]["tool_name"], "lookup");
        assert_eq!(messages[3]["tool_call_id"], "call_1");
        assert_eq!(messages[3]["content"], "looked up \"gap\"");
        drop(requests);

        // The next turn carries on from the whole exchange
        assert_eq!(conversation.body.messages.len(), 5);
        conversation.reset();
        assert_eq!(conversation.body.messages.len(), 1);
    }

    #[tokio::test]
    async fn thinks_only_on_models_that_can() {
        let show = |capabilities: Value| (200, json!({"capabilities": capabilities}).to_string());
        let (url, requests) = mock(vec![
            show(json!(["completion", "tools", "thinking"])),
            stream(&["Box."], Value::Null, "stop"),
            stream(&["Stay out."], Value::Null, "stop"),
        ])
        .await;
        let calls = Arc::new(AtomicUsize::new(0));
        let mut builder = builder(&url, &calls);
        builder.effort("max");
        let mut conversation = builder.build(reqwest::Client::new()).unwrap();
        send_streaming(&mut conversation, "Pit?").await.unwrap();
        send_streaming(&mut conversation, "Pit?").await.unwrap();

        let requests = requests.lock().unwrap();
        // Asked once
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].0, "/api/show");
        assert_eq!(requests[0].1["model"], "qwen3.5:9b");
        assert_eq!(requests[1].1["think"], "high");
        assert_eq!(requests[2].1["think"], "high");
        drop(requests);

        let (url, requests) = mock(vec![
            show(json!(["completion", "tools"])),
            stream(&["Box."], Value::Null, "stop"),
        ])
        .await;
        let mut builder = self::builder(&url, &calls);
        builder.effort("low");
        let mut conversation = builder.build(reqwest::Client::new()).unwrap();
        send_streaming(&mut conversation, "Pit?").await.unwrap();
        assert!(requests.lock().unwrap()[1].1.get("think").is_none());
    }

    #[tokio::test]
    async fn tells_cut_off_answers_from_a_full_conversation() {
        let (url, _) = mock(vec![
            stream(&["Box, box"], Value::Null, "length"),
            stream(&["Box"], Value::Null, "length"),
        ])
        .await;
        let calls = Arc::new(AtomicUsize::new(0));
        let mut conversation = builder(&url, &calls).build(reqwest::Client::new()).unwrap();
        let (reply, _) = send_streaming(&mut conversation, "Pit?").await.unwrap();
        assert_eq!(reply.stop, StopReason::MaxTokens);

        // 300 prompt and 20 written fill a 320-token window
        conversation.body.options.num_ctx = NonZeroU32::new(320).unwrap();
        let (reply, _) = send_streaming(&mut conversation, "Pit?").await.unwrap();
        assert_eq!(reply.stop, StopReason::ContextWindowExceeded);
    }

    #[tokio::test]
    async fn doesnt_run_tool_calls_that_were_cut_off() {
        let (url, _) = mock(vec![stream(
            &[],
            json!([{"function": {"name": "lookup", "arguments": {"what": "gap"}}}]),
            "length",
        )])
        .await;
        let calls = Arc::new(AtomicUsize::new(0));
        let mut conversation = builder(&url, &calls).build(reqwest::Client::new()).unwrap();
        let (reply, _) = send_streaming(&mut conversation, "Gap?").await.unwrap();
        assert_eq!(reply.stop, StopReason::MaxTokens);
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        // The question stays, without the calls
        assert_eq!(conversation.body.messages.len(), 2);
    }

    #[tokio::test]
    async fn reports_errors_and_forgets_the_exchange() {
        let (url, _) = mock(vec![
            (404, json!({"error": "model 'nope' not found"}).to_string()),
            (
                200,
                json!({"model": "qwen3.5:9b", "message": {"role": "assistant", "content": "Bo"}, "done": false}).to_string()
                    + "\n"
                    + &json!({"error": "out of memory"}).to_string()
                    + "\n",
            ),
            (200, json!({"model": "qwen3.5:9b", "message": {"role": "assistant", "content": "Bo"}, "done": false}).to_string()),
        ])
        .await;
        let calls = Arc::new(AtomicUsize::new(0));
        let mut conversation = builder(&url, &calls).build(reqwest::Client::new()).unwrap();

        let error = send_streaming(&mut conversation, "Pit?").await.unwrap_err();
        let error = error.downcast_ref::<ApiError>().unwrap();
        assert_eq!(error.status, StatusCode::NOT_FOUND);
        assert_eq!(error.message.as_deref(), Some("model 'nope' not found"));
        assert_eq!(conversation.body.messages.len(), 1);

        let error = send_streaming(&mut conversation, "Pit?").await.unwrap_err();
        assert!(format!("{error:#}").contains("out of memory"), "{error:#}");
        assert_eq!(conversation.body.messages.len(), 1);

        let error = send_streaming(&mut conversation, "Pit?").await.unwrap_err();
        assert!(format!("{error:#}").contains("ended before"), "{error:#}");
        assert_eq!(conversation.body.messages.len(), 1);
    }

    #[tokio::test]
    async fn forgets_cancelled_exchanges() {
        let (url, _) = mock(Vec::new()).await;
        let calls = Arc::new(AtomicUsize::new(0));
        let mut conversation = builder(&url, &calls).build(reqwest::Client::new()).unwrap();
        let cancelled = timeout(Duration::from_millis(200), conversation.send("Hi".into())).await;
        assert!(cancelled.is_err());
        assert_eq!(conversation.body.messages.len(), 1);
    }

    #[test]
    fn reads_the_host_like_ollama() {
        assert_eq!(host_url("0.0.0.0"), "http://127.0.0.1:11434");
        assert_eq!(host_url("192.168.1.5"), "http://192.168.1.5:11434");
        assert_eq!(host_url("pc:8080"), "http://pc:8080");
        assert_eq!(host_url("https://ollama.com"), "https://ollama.com:443");
        assert_eq!(host_url("http://[::1]"), "http://[::1]:11434");
        assert_eq!(host_url("http://[::1]:9000/"), "http://[::1]:9000");
    }

    /// Against the Ollama running on this PC, with a model that's been pulled (`OLLAMA_MODEL`,
    /// default `qwen3.5:9b`).
    #[tokio::test]
    #[ignore]
    async fn answers_from_ollama() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut builder = ConversationBuilder::new();
        builder
            .model(std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| "qwen3.5:9b".into()))
            .effort("none")
            .system("You are a race engineer. Use the lookup tool for anything about the car.")
            .tool(
                "lookup",
                Some("Looks up the car's telemetry: `what` is e.g. `gap`".into()),
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
        assert!(calls.load(Ordering::Relaxed) > 0);
        assert_eq!(pieces.concat(), reply.text);
    }
}
