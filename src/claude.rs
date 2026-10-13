use crate::claude::tool_handler::{ToolHandler, Tools};
use anyhow::bail;
use futures::future::join_all;
use reqwest::{
    RequestBuilder, StatusCode,
    header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue},
};
use serde::{Deserialize, Serialize};
use std::{
    env::VarError,
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

impl Conversation {
    pub async fn send(&mut self, text: String) -> anyhow::Result<()> {
        self.body.messages.push(RequestMessage {
            role: "user",
            content: serde_json::Value::String(text),
        });

        for _ in 0..self.max_requests.get() {
            let response = self.post().await?;
            if let Some(container_id) = response.container.and_then(|c| c.id) {
                self.body.container.get_or_insert_default().id = Some(container_id);
            }

            // Only client tool calls get results (server tools like code execution run on
            // Anthropic's side)
            let tools = &self.tools;
            let results = join_all(
                response
                    .content
                    .iter()
                    .filter(|block| block["type"] == "tool_use")
                    .map(|block| async move {
                        let name = block.get("name").and_then(serde_json::Value::as_str);
                        let input = block.get("input");

                        let (content, is_error) = match name.zip(input) {
                            Some((name, input)) => match tools.handle(name, input).await {
                                Ok(x) => (x, false),
                                Err(e) => (e.to_string(), true),
                            },
                            None => ("Invalid tool call request".into(), true),
                        };

                        ResultBody {
                            r#type: "tool_result",
                            tool_use_id: block["id"].clone(),
                            content,
                            is_error,
                        }
                    }),
            )
            .await;

            // The assistant's turn goes into the history exactly as received: tool results must
            // follow the turn with their `tool_use` blocks, a `pause_turn` resumes from it, and
            // thinking blocks are only valid in an unedited history
            if !response.content.is_empty() {
                self.body.messages.push(RequestMessage {
                    role: "assistant",
                    content: serde_json::Value::Array(response.content),
                });
            }

            match response.stop_reason.as_deref() {
                Some("pause_turn") => continue,
                Some("tool_use") if !results.is_empty() => {
                    self.body.messages.push(RequestMessage {
                        role: "user",
                        content: serde_json::to_value(results)?,
                    });
                    continue;
                }
                // FIXME(TODO)
                _ => return Ok(()),
            }
        }

        bail!("Claude did not finish its turn within ConversationBuilder::max_requests requests")
    }

    async fn post(&self) -> anyhow::Result<ResponseBody> {
        const RETRY_AFTER: HeaderName = HeaderName::from_static("retry-after");

        for i in 0..=self.max_retries {
            let resp = self.request().send().await?;
            if resp.status().is_success() {
                return Ok(resp.json().await?);
            }

            let retryable = resp.status().is_server_error()
                || matches!(
                    resp.status(),
                    StatusCode::REQUEST_TIMEOUT
                        | StatusCode::CONFLICT
                        | StatusCode::TOO_MANY_REQUESTS
                );

            if !retryable {
                break;
            }

            tokio::time::sleep(
                resp.headers()
                    .get(RETRY_AFTER)
                    .and_then(parse_retry_after)
                    .unwrap_or(Duration::from_millis(500).saturating_mul(1 << i)),
            )
            .await;
        }

        bail!("Claude request failed");
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
    cache_control: RequestCacheControl,
    messages: Vec<RequestMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<RequestTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    container: Option<RequestContainer>,
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
