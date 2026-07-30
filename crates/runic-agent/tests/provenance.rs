mod harness;

use std::sync::Arc;

use async_trait::async_trait;
use harness::*;
use runic_agent::Runner;
use runic_tool::{Tool, ToolContext, ToolResult};
use runic_types::{ContentBlock, MessageContent, ProvenanceSource};

struct SourcedTool;

#[async_trait]
impl Tool for SourcedTool {
    fn name(&self) -> &str {
        "sourced"
    }
    fn description(&self) -> &str {
        "returns an answer with provenance"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object" })
    }
    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        let sources = (0..20)
            .map(|i| {
                ProvenanceSource::new(
                    format!("s{i}"),
                    "https://user:pw@example.com/doc?page=1&token=SECRETTOKEN",
                )
                .with_snippet("supporting text")
            })
            .collect();
        Ok(ToolResult::ok("the answer").with_provenance(sources))
    }
}

#[tokio::test]
async fn provenance_is_sanitized_bounded_and_persisted_on_the_block() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_use_response("c1", "sourced", serde_json::json!({})),
        text_response("done"),
    ]));
    let mut agent = Runner::builder(provider.clone(), "t", "s")
        .model("test")
        .tool(Arc::new(SourcedTool))
        .build();

    let (events_tx, mut events_rx) =
        tokio::sync::mpsc::unbounded_channel::<runic_agent::AgentEvent>();
    agent
        .run_with(
            "go",
            runic_agent::RunContext::new()
                .with_events(std::sync::Arc::new(runic_agent::ChannelEmitter(events_tx))),
        )
        .await
        .unwrap();

    let block = agent
        .state()
        .messages_for_provider()
        .iter()
        .filter_map(|m| match &m.content {
            MessageContent::Blocks(blocks) => Some(blocks),
            _ => None,
        })
        .flatten()
        .find_map(|b| match b {
            ContentBlock::ToolResult { provenance, .. } if !provenance.is_empty() => {
                Some(provenance.clone())
            }
            _ => None,
        })
        .expect("provenance persisted on the block");

    assert_eq!(block.len(), runic_types::provenance::MAX_PROVENANCE_SOURCES);
    for source in &block {
        assert!(!source.source.contains("user:pw@"));
        assert!(!source.source.contains("SECRETTOKEN"));
        assert!(source.source.contains("page=1"));
    }

    let mut wire_provenance = None;
    while let Ok(event) = events_rx.try_recv() {
        if let runic_agent::AgentEvent::ToolFinished { provenance, .. } = event {
            wire_provenance = Some(provenance);
        }
    }
    let wire_provenance = wire_provenance.expect("ToolFinished event fired");
    assert_eq!(
        wire_provenance.len(),
        runic_types::provenance::MAX_PROVENANCE_SOURCES
    );
    assert!(!wire_provenance[0].source.contains("SECRETTOKEN"));
}
