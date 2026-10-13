use anyhow::bail;
use serde::de::DeserializeOwned;
use std::{collections::BTreeMap, pin::Pin};

pub trait ToolHandler {
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

    pub(super) async fn handle(
        &self,
        name: &str,
        input: &serde_json::Value,
    ) -> anyhow::Result<String> {
        let Some(handler) = self.handlers.get(name) else {
            bail!("No handler for this tool call");
        };
        return handler.handle(input).await;
    }
}

trait RawToolHandler {
    fn handle<'a>(
        &'a self,
        input: &serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<String>> + 'a>>;
}

impl<T> RawToolHandler for T
where
    T: ToolHandler,
{
    fn handle<'a>(
        &'a self,
        input: &serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<String>> + 'a>> {
        let input = input.clone();
        return Box::pin(async move {
            let input = serde_json::from_value::<T::Input>(input)?;
            return <T as ToolHandler>::handle(self, input).await;
        });
    }
}
