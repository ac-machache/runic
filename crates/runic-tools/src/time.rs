//! `system_time` — the current date/time. The model has no inherent sense of
//! "now"; this gives it one. Optional IANA timezone, else UTC.

use async_trait::async_trait;
use chrono::Utc;
use chrono_tz::Tz;

use runic_tool::{Tool, ToolContext, ToolResult};

pub struct SystemTimeTool;

#[async_trait]
impl Tool for SystemTimeTool {
    fn name(&self) -> &str {
        "system_time"
    }
    fn description(&self) -> &str {
        "The current date and time. Pass `timezone` (an IANA name like \
         \"America/New_York\") to localize it; defaults to UTC."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "timezone": { "type": "string", "description": "IANA tz name, e.g. Europe/Paris." }
            }
        })
    }
    fn parallelizable(&self) -> bool {
        true
    }
    async fn execute(&self, args: serde_json::Value, _ctx: &ToolContext) -> anyhow::Result<ToolResult> {
        let now = Utc::now();
        match args.get("timezone").and_then(|v| v.as_str()) {
            None => Ok(ToolResult::ok(now.format("%Y-%m-%d %H:%M:%S UTC").to_string())),
            Some(name) => match name.parse::<Tz>() {
                Ok(tz) => {
                    let local = now.with_timezone(&tz);
                    Ok(ToolResult::ok(local.format("%Y-%m-%d %H:%M:%S %Z (%:z)").to_string()))
                }
                Err(_) => Ok(ToolResult::error(format!("unknown timezone '{name}'"))),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use runic_tool::ToolContext;

    #[tokio::test]
    async fn returns_utc_and_localizes() {
        let ctx = ToolContext::new("u", "s", "r");
        let utc = SystemTimeTool
            .execute(serde_json::json!({}), &ctx)
            .await
            .unwrap();
        assert!(utc.success && utc.output.contains("UTC"));

        let paris = SystemTimeTool
            .execute(serde_json::json!({ "timezone": "Europe/Paris" }), &ctx)
            .await
            .unwrap();
        assert!(paris.success);

        let bad = SystemTimeTool
            .execute(serde_json::json!({ "timezone": "Nowhere/Nope" }), &ctx)
            .await
            .unwrap();
        assert!(!bad.success);
    }
}
