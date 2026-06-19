//! `MemoryConfig` — the knobs that shape the memory layer, mirroring hermes's
//! `[memory]` config section. The app fills this from its own config file /
//! request and hands it to the builder that wires the [`MemoryManager`].

use serde::{Deserialize, Serialize};

use crate::store::{DEFAULT_MEMORY_LIMIT, DEFAULT_USER_LIMIT};

/// Default turns between background memory-review nudges (hermes default).
pub const DEFAULT_NUDGE_INTERVAL: u32 = 10;

/// Top-level memory configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryConfig {
    /// Master switch for the whole memory layer.
    pub enabled: bool,
    /// Inject + allow writes to MEMORY.md (the agent's own notes).
    pub memory_enabled: bool,
    /// Inject + allow writes to USER.md (the user profile).
    pub user_profile_enabled: bool,
    /// Char cap for MEMORY.md.
    pub memory_char_limit: usize,
    /// Char cap for USER.md.
    pub user_char_limit: usize,
    /// Reject entries that trip the threat scanner (injection/exfil/unicode).
    pub threat_scanning: bool,
    /// Spawn a background memory-review every N user turns (0 disables it).
    pub nudge_interval: u32,
    /// Optional external memory provider layered alongside the built-in store.
    pub provider: ProviderConfig,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            memory_enabled: true,
            user_profile_enabled: true,
            memory_char_limit: DEFAULT_MEMORY_LIMIT,
            user_char_limit: DEFAULT_USER_LIMIT,
            threat_scanning: true,
            nudge_interval: DEFAULT_NUDGE_INTERVAL,
            provider: ProviderConfig::Builtin,
        }
    }
}

impl MemoryConfig {
    /// Whether the background-review nudge is active.
    pub fn nudge_enabled(&self) -> bool {
        self.enabled && self.nudge_interval > 0
    }
}

/// Which provider(s) back the layer. The built-in file store is always present
/// when `enabled`; an external provider is layered *alongside* it (hermes runs
/// one external at a time, mirroring built-in writes into it).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProviderConfig {
    /// Only the built-in bounded file store.
    #[default]
    Builtin,
    /// Built-in store + an external service (Honcho / Hindsight / Mem0 / …).
    External(ExternalProviderConfig),
}


/// Configuration for an external memory provider. `name` selects the
/// implementation; the rest is generic so a new provider needs no schema churn.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ExternalProviderConfig {
    /// Provider implementation name, e.g. "honcho", "hindsight", "mem0".
    pub name: String,
    /// Service base URL, if the provider is HTTP-backed.
    pub base_url: Option<String>,
    /// API key / token (the app is responsible for sourcing it securely).
    pub api_key: Option<String>,
    /// Provider-specific extra options, passed through verbatim.
    pub options: serde_json::Map<String, serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_hermes() {
        let c = MemoryConfig::default();
        assert!(c.enabled && c.memory_enabled && c.user_profile_enabled);
        assert_eq!(c.memory_char_limit, 2200);
        assert_eq!(c.user_char_limit, 1375);
        assert_eq!(c.nudge_interval, 10);
        assert!(matches!(c.provider, ProviderConfig::Builtin));
        assert!(c.nudge_enabled());
    }

    #[test]
    fn nudge_disabled_when_interval_zero() {
        let c = MemoryConfig { nudge_interval: 0, ..Default::default() };
        assert!(!c.nudge_enabled());
    }

    #[test]
    fn provider_config_roundtrips_through_json() {
        let json = serde_json::json!({
            "kind": "external",
            "name": "mem0",
            "base_url": "https://api.mem0.ai",
            "api_key": "sk-x",
            "options": { "org": "acme" }
        });
        let p: ProviderConfig = serde_json::from_value(json).unwrap();
        match p {
            ProviderConfig::External(e) => {
                assert_eq!(e.name, "mem0");
                assert_eq!(e.base_url.as_deref(), Some("https://api.mem0.ai"));
                assert_eq!(e.options["org"], "acme");
            }
            _ => panic!("expected external"),
        }
    }

    #[test]
    fn builtin_is_the_default_variant() {
        let p: ProviderConfig = serde_json::from_value(serde_json::json!({"kind": "builtin"})).unwrap();
        assert!(matches!(p, ProviderConfig::Builtin));
    }
}
