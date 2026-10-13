use anyhow::bail;
use serde::{Deserialize, de::DeserializeOwned};
use std::{collections::BTreeMap, pin::Pin};

pub trait ToolHandler: Send + Sync {
    type Input: DeserializeOwned;

    fn handle(
        &self,
        input: Self::Input,
    ) -> impl Future<Output = anyhow::Result<String>> + Send + 'static;
}

#[derive(Default)]
pub struct Tools {
    handlers: BTreeMap<&'static str, Box<dyn RawToolHandler>>,
}

impl Tools {
    #[inline]
    pub fn insert<H>(&mut self, tool: &'static str, handler: H)
    where
        H: ToolHandler + 'static,
    {
        self.handlers.insert(tool, Box::new(handler));
    }

    pub(super) fn handle(
        &self,
        name: &str,
        input: &serde_json::Value,
    ) -> impl Future<Output = anyhow::Result<String>> + Send + 'static {
        let call = match self.handlers.get(name) {
            Some(handler) => handler.handle(input),
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
