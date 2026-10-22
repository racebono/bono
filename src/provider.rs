//! The language models the race engineer talks through, whoever runs them: a [`Model`] is one
//! conversation with one, tool calls included. [`claude`] is Claude's (the Claude API),
//! [`ollama`] any model Ollama runs, and [`bono_cloud`] whichever Bono's website runs for the
//! signed-in account; others (e.g. OpenAI's) implement the same trait.

use crate::{
    account::Account,
    config::{ModelConfig, ModelProvider},
};
use futures::future::BoxFuture;
pub use openai::DEFAULT_BASE_URL as OPENAI_DEFAULT_BASE_URL;
use std::{collections::BTreeMap, ops::AddAssign, sync::Arc};

pub mod bono_cloud;
pub mod claude;
pub mod ollama;
pub mod openai;
mod text;
mod tool;

pub use tool::{ToolHandler, Tools};

/// A model a provider offers.
#[derive(Debug, Clone)]
pub struct ModelOption {
    /// What it's asked for by, e.g. `claude-sonnet-5-5` or `qwen3.5:9b`
    pub name: String,
    /// Its size on disk, in bytes, for one run on this PC (or another)
    pub size: Option<u64>,
}

/// The models the provider `config` picks offers (for Ollama, the ones pulled at its host; for
/// an OpenAI-compatible API, the ones at its URL; for Bono Cloud, the ones `account` may use).
pub async fn models(
    config: &ModelConfig,
    account: Option<&Account>,
    client: &reqwest::Client,
) -> anyhow::Result<Vec<ModelOption>> {
    match config.provider() {
        ModelProvider::BonoCloud => bono_cloud::models(account, client).await,
        ModelProvider::Claude => claude::models(&config.claude, client).await,
        ModelProvider::Ollama => ollama::models(config.ollama.host().as_deref(), client).await,
        ModelProvider::OpenAi => {
            let openai = &config.openai;
            openai::models(
                openai.base_url().as_deref(),
                openai.api_key().as_deref(),
                client,
            )
            .await
        }
    }
}

/// A conversation with a language model.
pub trait Model: Send {
    /// Sends the user's message and runs the model's turn to its end, tool calls included, with
    /// the reply's text handed to `on_text` as the model writes it: put together, the pieces are
    /// [`Reply::text`]. Text already handed over stays said, even if the turn then fails, or ends
    /// in a refusal. On an error, or if the returned future is dropped before it finishes, the
    /// conversation is left as it was before the call.
    fn send_streaming<'a>(
        &'a mut self,
        text: String,
        on_text: &'a mut (dyn FnMut(&str) + Send),
    ) -> BoxFuture<'a, anyhow::Result<Reply>>;

    /// Like [`Self::send_streaming`], without the text as it comes.
    #[cfg_attr(not(test), allow(dead_code))]
    fn send(&mut self, text: String) -> BoxFuture<'_, anyhow::Result<Reply>> {
        Box::pin(async move { self.send_streaming(text, &mut |_: &str| {}).await })
    }

    /// Starts the conversation over, e.g. for a new session: forgets every message. The settings
    /// and tools stay.
    fn reset(&mut self);

    /// Every turn's tokens so far (resets included), by the model that answered it.
    #[allow(dead_code)]
    fn usage(&self) -> &BTreeMap<String, Usage>;
}

/// A conversation with the model `config` picks, with the `system` prompt and `tools`. Bono
/// Cloud's is as `account`.
pub fn conversation(
    config: &ModelConfig,
    account: Option<&Account>,
    system: Arc<str>,
    tools: Tools,
    client: reqwest::Client,
) -> anyhow::Result<Box<dyn Model>> {
    Ok(match config.provider() {
        ModelProvider::BonoCloud => Box::new(bono_cloud::Conversation::new(
            account, system, tools, client,
        )?),
        ModelProvider::Claude => Box::new(claude::Conversation::new(
            &config.claude,
            system,
            tools,
            client,
        )?),
        ModelProvider::Ollama => Box::new(ollama::Conversation::new(
            &config.ollama,
            system,
            tools,
            client,
        )?),
        ModelProvider::OpenAi => Box::new(openai::Conversation::new(
            &config.openai,
            system,
            tools,
            client,
        )?),
    })
}

/// What the model answered to one [`Model::send`].
#[derive(Debug, Clone)]
pub struct Reply {
    /// Every text block of the turn, in order (with tool calls, the model may write some before
    /// the calls and the rest after), separated by blank lines.
    pub text: String,
    pub stop: StopReason,
    /// The tokens of every request the turn took.
    pub usage: Usage,
    /// The model that wrote the turn's end: with fallbacks, not always the one asked for.
    pub model: String,
}

/// Why the model's turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// It finished its answer.
    EndTurn,
    /// It wrote one of the request's stop sequences.
    StopSequence,
    /// The answer was cut off at the most tokens it may write: `text` is incomplete.
    MaxTokens,
    /// The conversation no longer fits in the model's context window: `text` is incomplete, and
    /// every later turn would end the same way, so it needs to start over.
    ContextWindowExceeded,
    /// The model (and every fallback model) declined. The exchange isn't kept in the
    /// conversation, as if it never happened.
    Refusal,
}

/// Tokens, by kind. Models without a prompt cache count all their input as `input_tokens`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    /// Read without the cache
    pub input_tokens: u64,
    /// Written to the cache
    pub cache_write_tokens: u64,
    /// Read from the cache
    pub cache_read_tokens: u64,
    pub output_tokens: u64,
}

impl Usage {
    /// Every token read, cached or not.
    #[inline]
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn total_input_tokens(&self) -> u64 {
        self.input_tokens + self.cache_write_tokens + self.cache_read_tokens
    }
}

impl AddAssign for Usage {
    fn add_assign(&mut self, other: Self) {
        self.input_tokens += other.input_tokens;
        self.cache_write_tokens += other.cache_write_tokens;
        self.cache_read_tokens += other.cache_read_tokens;
        self.output_tokens += other.output_tokens;
    }
}
