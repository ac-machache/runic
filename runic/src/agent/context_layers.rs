//! Context layers for the coral agent's system-prompt assembly.
//!
//! [`DateLayer`] is runic's equivalent of coral's `CoralPromptMiddleware`:
//! it injects today's date (in a fixed timezone) so the model can do
//! relative-date math ("les 7 derniers mois") without guessing what day it
//! is, plus the TC's name when the request carried one. It plugs into a
//! `CompositeEngine` like any other `ContextLayer`.

use async_trait::async_trait;
use chrono_tz::Tz;
use runic_context_engine::{ContextLayer, TurnContext};

/// Per-run config key carrying the technicien-commercial's display name.
const KEY_TC_NAME: &str = "tc_name";

/// Injects the current date (and optionally the TC's name, read per-run
/// from `TurnContext.config`) as a small `<contexte>` block. Renders every
/// turn, so the date stays correct across a long-lived thread and the name
/// reflects whoever made the request.
pub struct DateLayer {
    tz: Tz,
}

impl DateLayer {
    /// Default to Europe/Paris — the coral agent's operating timezone.
    pub fn new() -> Self {
        Self {
            tz: chrono_tz::Europe::Paris,
        }
    }

    /// Rendered separately from `now()` / `ctx` so tests can pin the inputs.
    fn render_for(&self, now_utc: chrono::DateTime<chrono::Utc>, tc_name: Option<&str>) -> String {
        let local = now_utc.with_timezone(&self.tz);
        let date = local.format("%Y-%m-%d");
        let mut out = format!("<contexte>\nDate du jour : {date} ({}).", self.tz.name());
        if let Some(name) = tc_name
            && !name.trim().is_empty()
        {
            out.push_str(&format!("\nTC : {name}."));
        }
        out.push_str("\n</contexte>");
        out
    }
}

impl Default for DateLayer {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ContextLayer for DateLayer {
    fn name(&self) -> &str {
        "date"
    }

    async fn render(&self, ctx: &TurnContext<'_>) -> Option<String> {
        let tc_name = ctx.config.get(KEY_TC_NAME).and_then(|v| v.as_str());
        Some(self.render_for(chrono::Utc::now(), tc_name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn renders_date_in_paris_timezone() {
        let layer = DateLayer::new();
        // 2026-06-14 00:30 UTC is already 02:30 in Paris (CEST, summer) —
        // same calendar day, sanity-checks the tz conversion path.
        let instant = chrono::Utc.with_ymd_and_hms(2026, 6, 14, 0, 30, 0).unwrap();
        let out = layer.render_for(instant, None);
        assert!(out.contains("Date du jour : 2026-06-14"));
        assert!(out.contains("Europe/Paris"));
        assert!(out.starts_with("<contexte>"));
        assert!(out.trim_end().ends_with("</contexte>"));
    }

    #[test]
    fn day_boundary_rolls_over_into_paris_local_day() {
        // 2026-06-14 23:30 UTC is 2026-06-15 01:30 in Paris — the date
        // block must show the LOCAL day, not the UTC one.
        let layer = DateLayer::new();
        let instant = chrono::Utc.with_ymd_and_hms(2026, 6, 14, 23, 30, 0).unwrap();
        let out = layer.render_for(instant, None);
        assert!(out.contains("2026-06-15"), "{out}");
    }

    #[test]
    fn includes_tc_name_when_present() {
        let layer = DateLayer::new();
        let instant = chrono::Utc.with_ymd_and_hms(2026, 6, 14, 9, 0, 0).unwrap();
        let out = layer.render_for(instant, Some("Marc"));
        assert!(out.contains("TC : Marc."));
    }

    #[test]
    fn blank_tc_name_is_ignored() {
        let layer = DateLayer::new();
        let instant = chrono::Utc.with_ymd_and_hms(2026, 6, 14, 9, 0, 0).unwrap();
        let out = layer.render_for(instant, Some("   "));
        assert!(!out.contains("TC :"));
    }

    #[tokio::test]
    async fn reads_tc_name_from_per_run_config() {
        let layer = DateLayer::new();
        let mut config = serde_json::Map::new();
        config.insert("tc_name".into(), serde_json::json!("Sophie"));
        let ctx = TurnContext {
            base_system_prompt: "",
            messages: &[],
            run_id: "r1",
            turn: 0,
            config: &config,
        };
        let out = layer.render(&ctx).await.unwrap();
        assert!(out.contains("TC : Sophie."));
    }
}
