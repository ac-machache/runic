use std::sync::Arc;

use runic_macros::ability;

use super::client::WebClient;
use super::fetch::{DEFAULT_MAX_LENGTH, Format, WebFetchTool};
use super::search::{SearchProvider, WebSearchTool};
use crate::ability::{Ability, BuildCtx};

#[ability]
pub struct Web {
    http: WebClient,
    format: Format,
    max_length: usize,
    max_results: usize,
    search: Option<Arc<dyn SearchProvider>>,
}

impl Default for Web {
    fn default() -> Self {
        Self::new()
    }
}

impl Web {
    pub fn new() -> Self {
        Self {
            http: WebClient::new(),
            format: Format::default(),
            max_length: DEFAULT_MAX_LENGTH,
            max_results: 5,
            search: None,
        }
    }

    pub fn client(mut self, http: WebClient) -> Self {
        self.http = http;
        self
    }

    pub fn raw(mut self) -> Self {
        self.format = Format::Raw;
        self
    }

    pub fn markdown(mut self) -> Self {
        self.format = Format::Markdown;
        self
    }

    pub fn max_length(mut self, chars: usize) -> Self {
        self.max_length = chars.max(1);
        self
    }

    pub fn search(mut self, provider: impl SearchProvider + 'static) -> Self {
        self.search = Some(Arc::new(provider));
        self
    }

    pub fn search_arc(mut self, provider: Arc<dyn SearchProvider>) -> Self {
        self.search = Some(provider);
        self
    }

    pub fn max_results(mut self, results: usize) -> Self {
        self.max_results = results;
        self
    }

    async fn ability(&self, base: Ability, _ctx: &BuildCtx<'_>) -> anyhow::Result<Ability> {
        let mut ability = base.describe("read and search the open web").tool(
            WebFetchTool::new(self.http.clone())
                .format(self.format)
                .max_length(self.max_length),
        );
        if let Some(provider) = &self.search {
            ability = ability.tool(
                WebSearchTool::new(self.http.clone(), provider.clone())
                    .with_max_results(self.max_results),
            );
        }
        Ok(ability)
    }
}
