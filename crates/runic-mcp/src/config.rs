//! Config: `~/.runic/mcp.json` parsing.
//!
//! Wire format matches Claude Desktop / Cursor's `mcpServers` shape exactly
//! so existing user configs paste in without modification.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McpConfig {
    /// Map of server-name → server-config. Server name is what shows up in
    /// the prefixed tool name (`mcp__{server}__{tool}`).
    #[serde(rename = "mcpServers", default)]
    pub mcp_servers: HashMap<String, McpServerConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// Executable to spawn (resolved via PATH unless absolute).
    pub command: String,
    /// Arguments passed verbatim.
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra env vars to pass to the child. Merged on top of the parent
    /// process's environment.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// When `true` (default), this server can be reused across sessions via
    /// `SharedMcpPool`. Set `false` for stateful servers (browser sessions,
    /// IDE handles, etc.) that must be spawned per-session.
    #[serde(default = "default_shared")]
    pub shared: bool,
}

fn default_shared() -> bool {
    true
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("could not read config at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("could not parse config at {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

impl McpConfig {
    /// Read & parse a config from a specific path. Returns `Err` if the file
    /// is missing — use [`Self::try_load_from_path`] if missing is OK.
    pub async fn load_from_path(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let p: &Path = path.as_ref();
        let raw = tokio::fs::read_to_string(p)
            .await
            .map_err(|source| ConfigError::Io {
                path: p.to_path_buf(),
                source,
            })?;
        Self::parse(&raw, p)
    }

    /// Like [`Self::load_from_path`] but returns `Ok(None)` when the file is
    /// absent (other I/O errors still propagate).
    pub async fn try_load_from_path(
        path: impl AsRef<Path>,
    ) -> Result<Option<Self>, ConfigError> {
        let p: &Path = path.as_ref();
        match tokio::fs::read_to_string(p).await {
            Ok(raw) => Self::parse(&raw, p).map(Some),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(ConfigError::Io {
                path: p.to_path_buf(),
                source,
            }),
        }
    }

    fn parse(raw: &str, path: &Path) -> Result<Self, ConfigError> {
        serde_json::from_str(raw).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.mcp_servers.is_empty()
    }

    pub fn len(&self) -> usize {
        self.mcp_servers.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn parses_claude_desktop_shape() {
        let raw = r#"{
            "mcpServers": {
                "github": {
                    "command": "npx",
                    "args": ["-y", "@modelcontextprotocol/server-github"],
                    "env": { "GITHUB_TOKEN": "ghp_abc" }
                },
                "filesystem": {
                    "command": "uvx",
                    "args": ["mcp-server-filesystem", "/tmp"]
                }
            }
        }"#;
        let cfg = McpConfig::parse(raw, Path::new("test.json")).unwrap();
        assert_eq!(cfg.len(), 2);

        let github = cfg.mcp_servers.get("github").unwrap();
        assert_eq!(github.command, "npx");
        assert_eq!(github.args, vec!["-y", "@modelcontextprotocol/server-github"]);
        assert_eq!(github.env.get("GITHUB_TOKEN").map(String::as_str), Some("ghp_abc"));
        assert!(github.shared, "shared defaults to true");

        let fs = cfg.mcp_servers.get("filesystem").unwrap();
        assert_eq!(fs.command, "uvx");
        assert!(fs.env.is_empty());
    }

    #[test]
    fn empty_object_parses_as_empty_config() {
        let cfg = McpConfig::parse("{}", Path::new("t.json")).unwrap();
        assert!(cfg.is_empty());
    }

    #[test]
    fn empty_mcp_servers_object_is_fine() {
        let cfg = McpConfig::parse(r#"{"mcpServers":{}}"#, Path::new("t.json")).unwrap();
        assert!(cfg.is_empty());
    }

    #[test]
    fn shared_false_is_respected() {
        let raw = r#"{
            "mcpServers": {
                "browser": {
                    "command": "node",
                    "args": ["server.js"],
                    "shared": false
                }
            }
        }"#;
        let cfg = McpConfig::parse(raw, Path::new("t.json")).unwrap();
        assert!(!cfg.mcp_servers.get("browser").unwrap().shared);
    }

    #[test]
    fn round_trips_through_serde() {
        let mut cfg = McpConfig::default();
        cfg.mcp_servers.insert(
            "x".into(),
            McpServerConfig {
                command: "echo".into(),
                args: vec!["hi".into()],
                env: HashMap::new(),
                shared: true,
            },
        );
        let json = serde_json::to_string(&cfg).unwrap();
        let parsed: McpConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed.mcp_servers.get("x").unwrap().command, "echo");
    }

    #[test]
    fn malformed_json_yields_parse_error() {
        let err = McpConfig::parse("{not json", Path::new("/oops")).unwrap_err();
        match err {
            ConfigError::Parse { path, .. } => {
                assert_eq!(path, PathBuf::from("/oops"));
            }
            other => panic!("expected Parse error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn load_from_path_reads_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        tokio::fs::write(&path, r#"{"mcpServers":{}}"#).await.unwrap();
        let cfg = McpConfig::load_from_path(&path).await.unwrap();
        assert!(cfg.is_empty());
    }

    #[tokio::test]
    async fn try_load_returns_none_for_missing_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("does-not-exist.json");
        let cfg = McpConfig::try_load_from_path(&path).await.unwrap();
        assert!(cfg.is_none());
    }

    #[tokio::test]
    async fn try_load_propagates_non_notfound_errors() {
        // Path that exists but is a directory — reading it as a file errors
        // with something other than NotFound.
        let dir = tempdir().unwrap();
        let result = McpConfig::try_load_from_path(dir.path()).await;
        assert!(result.is_err());
    }
}
