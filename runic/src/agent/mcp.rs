use runic_mcp::{McpConfig, McpManager};
use std::path::Path;

pub async fn load(config_path: &Path) -> McpManager {
    match McpConfig::try_load_from_path(config_path).await {
        Ok(Some(cfg)) if !cfg.is_empty() => McpManager::connect_all(&cfg).await,
        _ => McpManager::new(),
    }
}
