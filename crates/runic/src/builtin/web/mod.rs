mod ability;
mod client;
mod fetch;
mod search;
mod text;

pub use ability::Web;
pub use client::{Fetched, WebClient, is_blocked_ip};
pub use fetch::{DEFAULT_MAX_LENGTH, Format, WebFetchTool};
pub use search::{SearchProvider, SearchResult, SearxngProvider, TavilyProvider, WebSearchTool};

#[doc(hidden)]
pub use text::{decode_entities, html_to_text};
