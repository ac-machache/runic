use chrono::Utc;
use chrono_tz::Tz;
use runic_macros::tool;
use runic_tool::{ToolContext, ToolResult};

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct SystemTimeArgs {
    #[serde(default)]
    #[schemars(with = "String", description = "IANA tz name, e.g. Europe/Paris.")]
    timezone: Option<String>,
}

#[tool(
    name = "system_time",
    args = SystemTimeArgs,
    execution = parallel,
    description = "The current date and time. Pass `timezone` (an IANA name like \
                   \"America/New_York\") to localize it; defaults to UTC."
)]
pub struct SystemTimeTool;

impl SystemTimeTool {
    async fn tool(&self, args: SystemTimeArgs, _ctx: &ToolContext) -> anyhow::Result<ToolResult> {
        let now = Utc::now();
        match args.timezone.as_deref() {
            None => Ok(ToolResult::ok(
                now.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
            )),
            Some(name) => match name.parse::<Tz>() {
                Ok(tz) => Ok(ToolResult::ok(
                    now.with_timezone(&tz)
                        .format("%Y-%m-%d %H:%M:%S %Z (%:z)")
                        .to_string(),
                )),
                Err(_) => Ok(ToolResult::error(format!("unknown timezone `{name}`"))),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use runic_tool::{Tool, ToolContext};

    #[tokio::test]
    async fn returns_utc_and_localizes() {
        let ctx = ToolContext::new("u", "s", "r");
        let utc = SystemTimeTool
            .execute(serde_json::json!({}), &ctx)
            .await
            .unwrap();
        assert!(!utc.is_error() && utc.text().contains("UTC"));

        let paris = SystemTimeTool
            .execute(serde_json::json!({ "timezone": "Europe/Paris" }), &ctx)
            .await
            .unwrap();
        assert!(!paris.is_error());

        let bad = SystemTimeTool
            .execute(serde_json::json!({ "timezone": "Nowhere/Nope" }), &ctx)
            .await
            .unwrap();
        assert!(bad.is_error());
    }
}
