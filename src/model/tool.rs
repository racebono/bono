use anyhow::bail;
use serde::{Deserialize, de::DeserializeOwned};
use std::pin::Pin;

/// A tool the model can call, run on this side.
pub trait ToolHandler: Send + Sync {
    type Input: DeserializeOwned;

    fn handle(
        &self,
        input: Self::Input,
    ) -> impl Future<Output = anyhow::Result<String>> + Send + 'static;
}

/// A conversation's tools: what the model is told about each, and what runs it.
#[derive(Default)]
pub struct Tools {
    /// In the order they were added
    tools: Vec<Tool>,
}

struct Tool {
    name: &'static str,
    description: Option<String>,
    input_schema: serde_json::Value,
    handler: Box<dyn RawToolHandler>,
}

/// A tool as the model is told about it.
pub struct ToolDefinition<'a> {
    pub name: &'static str,
    pub description: Option<&'a str>,
    /// A JSON Schema of its input
    pub input_schema: &'a serde_json::Value,
}

impl Tools {
    /// Adds a tool, or replaces the one with the same name.
    pub fn insert<H>(
        &mut self,
        name: &'static str,
        description: Option<String>,
        input_schema: serde_json::Value,
        handler: H,
    ) where
        H: ToolHandler + 'static,
    {
        let tool = Tool {
            name,
            description,
            input_schema,
            handler: Box::new(handler),
        };
        match self.tools.iter_mut().find(|tool| tool.name == name) {
            Some(existing) => *existing = tool,
            None => self.tools.push(tool),
        }
    }

    /// Every tool, as the model is told about it.
    pub fn definitions(&self) -> impl Iterator<Item = ToolDefinition<'_>> {
        self.tools.iter().map(|tool| ToolDefinition {
            name: tool.name,
            description: tool.description.as_deref(),
            input_schema: &tool.input_schema,
        })
    }

    /// Runs the model's call of the tool `name`. The future doesn't borrow the tools or the input.
    pub fn handle(
        &self,
        name: &str,
        input: &serde_json::Value,
    ) -> impl Future<Output = anyhow::Result<String>> + Send + use<> {
        let call = match self.tools.iter().find(|tool| tool.name == name) {
            Some(tool) => tool.handler.handle(input),
            None => Box::pin(async { bail!("No handler for this tool call") }),
        };
        return call;
    }
}

type ToolFuture = Pin<Box<dyn Future<Output = anyhow::Result<String>> + Send + 'static>>;

trait RawToolHandler: Send + Sync {
    fn handle(&self, input: &serde_json::Value) -> ToolFuture;
}

impl<T> RawToolHandler for T
where
    T: ToolHandler,
{
    fn handle(&self, input: &serde_json::Value) -> ToolFuture {
        // Parsed before the call, so the future doesn't borrow the handler or the input
        match T::Input::deserialize(input) {
            Ok(input) => Box::pin(<T as ToolHandler>::handle(self, input)),
            Err(e) => Box::pin(std::future::ready(Err(e.into()))),
        }
    }
}
