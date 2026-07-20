mod harness;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use harness::*;
use runic_agent::{Session, SpilledArtifact, ToolOutputSpill};
use runic_tool::{Tool, ToolContext, ToolResult};
use runic_types::{ContentBlock, MessageContent, ProvenanceSource, ToolResultPayload};

type SpillPut = (String, String, String, Vec<u8>);

struct FakeSpill {
    puts: Mutex<Vec<SpillPut>>,
    fail: bool,
}

impl FakeSpill {
    fn new() -> Self {
        Self {
            puts: Mutex::new(Vec::new()),
            fail: false,
        }
    }

    fn failing() -> Self {
        Self {
            puts: Mutex::new(Vec::new()),
            fail: true,
        }
    }

    fn puts(&self) -> Vec<SpillPut> {
        self.puts.lock().unwrap().clone()
    }
}

#[async_trait]
impl ToolOutputSpill for FakeSpill {
    async fn store(
        &self,
        tenant: &str,
        session: &str,
        mime: &str,
        bytes: &[u8],
    ) -> anyhow::Result<SpilledArtifact> {
        if self.fail {
            anyhow::bail!("disk full");
        }
        self.puts.lock().unwrap().push((
            tenant.to_string(),
            session.to_string(),
            mime.to_string(),
            bytes.to_vec(),
        ));
        Ok(SpilledArtifact {
            id: format!("art-{}", self.puts.lock().unwrap().len()),
            mime: mime.to_string(),
            size: bytes.len() as u64,
        })
    }
}

struct SpillingTool {
    output: serde_json::Value,
}

#[async_trait]
impl Tool for SpillingTool {
    fn name(&self) -> &str {
        "spiller"
    }
    fn description(&self) -> &str {
        "returns a payload marked for artifact retention"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object" })
    }
    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        Ok(ToolResult::ok(self.output.clone()).spill())
    }
}

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

fn persisted_payloads(agent: &Session) -> Vec<ToolResultPayload> {
    agent
        .state()
        .messages_for_provider()
        .iter()
        .filter_map(|m| match &m.content {
            MessageContent::Blocks(blocks) => Some(blocks),
            _ => None,
        })
        .flatten()
        .filter_map(|b| match b {
            ContentBlock::ToolResult { content, .. } => Some(content.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn artifact_retention_spills_bytes_and_persists_the_reference() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_use_response("c1", "spiller", serde_json::json!({})),
        text_response("done"),
    ]));
    let spill = Arc::new(FakeSpill::new());
    let output = serde_json::json!({ "rows": ["a", "b", "c"] });
    let mut agent = Session::builder(provider.clone(), "tenant-1", "session-1")
        .model("test")
        .tool(Arc::new(SpillingTool {
            output: output.clone(),
        }))
        .artifact_spill(spill.clone())
        .build();

    agent.run("go").await.unwrap();

    let puts = spill.puts();
    assert_eq!(puts.len(), 1);
    let (tenant, session, mime, bytes) = &puts[0];
    assert_eq!(tenant, "tenant-1");
    assert_eq!(session, "session-1");
    assert_eq!(mime, "application/json");
    assert_eq!(bytes, output.to_string().as_bytes());

    let payloads = persisted_payloads(&agent);
    let artifact = payloads
        .iter()
        .find(|p| matches!(p, ToolResultPayload::Artifact { .. }))
        .expect("history keeps the artifact arm");
    let ToolResultPayload::Artifact {
        id,
        preview,
        mime,
        size,
    } = artifact
    else {
        unreachable!()
    };
    assert_eq!(id, "art-1");
    assert_eq!(mime, "application/json");
    assert_eq!(*size, output.to_string().len() as u64);
    assert!(preview.starts_with(r#"{"rows""#));

    let followup = tool_result_contents(&provider.requests()[1].messages);
    assert!(
        followup.iter().any(|c| c == &output.to_string()),
        "the immediate next model call still sees the full output: {followup:?}"
    );
}

#[tokio::test]
async fn spill_failure_degrades_to_an_inline_note_never_the_full_bytes() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_use_response("c1", "spiller", serde_json::json!({})),
        text_response("done"),
    ]));
    let big = "SECRET_PAYLOAD ".repeat(100);
    let mut agent = Session::builder(provider.clone(), "t", "s")
        .model("test")
        .tool(Arc::new(SpillingTool {
            output: serde_json::json!(big),
        }))
        .artifact_spill(Arc::new(FakeSpill::failing()))
        .build();

    agent.run("go").await.unwrap();

    let payloads = persisted_payloads(&agent);
    let ToolResultPayload::Inline(serde_json::Value::String(note)) = &payloads[0] else {
        panic!("expected an inline fallback note, got {payloads:?}");
    };
    assert!(note.starts_with("[artifact spill failed: disk full]"));
    assert!(
        note.len() < big.len(),
        "the note is a preview, not the payload"
    );

    let followup = tool_result_contents(&provider.requests()[1].messages);
    assert!(
        followup.iter().any(|c| c == &big),
        "the model still gets the full output once"
    );
}

#[tokio::test]
async fn artifact_retention_without_a_store_degrades_the_same_way() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_use_response("c1", "spiller", serde_json::json!({})),
        text_response("done"),
    ]));
    let mut agent = Session::builder(provider, "t", "s")
        .model("test")
        .tool(Arc::new(SpillingTool {
            output: serde_json::json!("payload"),
        }))
        .build();

    agent.run("go").await.unwrap();

    let payloads = persisted_payloads(&agent);
    let ToolResultPayload::Inline(serde_json::Value::String(note)) = &payloads[0] else {
        panic!("expected an inline fallback note, got {payloads:?}");
    };
    assert!(note.contains("no artifact store wired"));
}

#[tokio::test]
async fn auto_spill_over_spills_only_full_retention_outputs_above_the_threshold() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        multi_tool_response(vec![
            ("c1", "small", serde_json::json!({})),
            ("c2", "big", serde_json::json!({})),
        ]),
        text_response("done"),
    ]));
    let spill = Arc::new(FakeSpill::new());
    let mut agent = Session::builder(provider, "t", "s")
        .model("test")
        .tool(Arc::new(RecordingTool::new("small", "tiny")))
        .tool(Arc::new(RecordingTool::new("big", &"waffle ".repeat(100))))
        .artifact_spill(spill.clone())
        .auto_spill_over(128)
        .build();

    agent.run("go").await.unwrap();

    assert_eq!(spill.puts().len(), 1, "only the big output spilled");
    let payloads = persisted_payloads(&agent);
    assert!(payloads.iter().any(
        |p| matches!(p, ToolResultPayload::Inline(serde_json::Value::String(s)) if s.starts_with("tiny"))
    ));
    assert!(
        payloads
            .iter()
            .any(|p| matches!(p, ToolResultPayload::Artifact { .. }))
    );
}

#[tokio::test]
async fn provenance_is_sanitized_bounded_and_persisted_on_the_block() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_use_response("c1", "sourced", serde_json::json!({})),
        text_response("done"),
    ]));
    let mut agent = Session::builder(provider.clone(), "t", "s")
        .model("test")
        .tool(Arc::new(SourcedTool))
        .build();

    let (events_tx, mut events_rx) =
        tokio::sync::mpsc::unbounded_channel::<runic_agent::AgentEvent>();
    agent
        .run_with("go", runic_agent::RunContext::new().with_events(events_tx))
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

struct SequenceSummaryTool {
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl Tool for SequenceSummaryTool {
    fn name(&self) -> &str {
        "seq"
    }
    fn description(&self) -> &str {
        "returns a distinct summarized output per invocation"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object" })
    }
    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        let nth = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(ToolResult::ok(format!("full-{nth}")).with_summary(format!("summary-{nth}")))
    }
}

struct DeferringTool;

#[async_trait]
impl Tool for DeferringTool {
    fn name(&self) -> &str {
        "deferrer"
    }
    fn description(&self) -> &str {
        "suspends the run"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object" })
    }
    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        Ok(ToolResult::defer("test", serde_json::json!({})))
    }
}

#[tokio::test]
async fn duplicate_call_ids_pair_each_block_with_its_own_full_output() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        multi_tool_response(vec![
            ("dup", "seq", serde_json::json!({})),
            ("dup", "seq", serde_json::json!({})),
        ]),
        text_response("done"),
    ]));
    let mut agent = Session::builder(provider.clone(), "t", "s")
        .model("test")
        .tool(Arc::new(SequenceSummaryTool {
            calls: std::sync::atomic::AtomicUsize::new(0),
        }))
        .build();

    agent.run("go").await.unwrap();

    let followup = tool_result_contents(&provider.requests()[1].messages);
    assert_eq!(
        followup,
        vec!["full-0".to_string(), "full-1".to_string()],
        "each duplicate-id block must receive its own output, in order"
    );
}

#[tokio::test]
async fn a_suspending_batch_spills_summarized_outputs_instead_of_losing_them() {
    let provider = Arc::new(ScriptedProvider::new(vec![multi_tool_response(vec![
        ("c1", "summary_tool", serde_json::json!({})),
        ("c2", "deferrer", serde_json::json!({})),
    ])]));
    let spill = Arc::new(FakeSpill::new());
    let mut agent = Session::builder(provider, "t", "s")
        .model("test")
        .tool(Arc::new(SummaryTool::new(
            "THE_FULL_BYTES",
            "short summary",
        )))
        .tool(Arc::new(DeferringTool))
        .artifact_spill(spill.clone())
        .build();

    let outcome = agent.run("go").await.unwrap();
    assert_eq!(outcome.stop_reason.as_deref(), Some("suspended"));

    assert_eq!(spill.puts().len(), 1, "the summarized output was spilled");
    let payloads = persisted_payloads(&agent);
    let artifact = payloads
        .iter()
        .find(|p| matches!(p, ToolResultPayload::Artifact { .. }))
        .unwrap_or_else(|| {
            panic!("history keeps a durable artifact ref, not a lossy summary: {payloads:?}")
        });
    let ToolResultPayload::Artifact { preview, .. } = artifact else {
        unreachable!()
    };
    assert_eq!(
        preview, "short summary",
        "the artifact preview is the tool's summary, never the full output"
    );
    assert!(
        !payloads.iter().any(|p| p.text().contains("THE_FULL_BYTES")),
        "the full output must not leak into the log via the preview: {payloads:?}"
    );
}

#[tokio::test]
async fn a_suspending_batch_without_a_store_keeps_the_summary() {
    let provider = Arc::new(ScriptedProvider::new(vec![multi_tool_response(vec![
        ("c1", "summary_tool", serde_json::json!({})),
        ("c2", "deferrer", serde_json::json!({})),
    ])]));
    let mut agent = Session::builder(provider, "t", "s")
        .model("test")
        .tool(Arc::new(SummaryTool::new(
            "THE_FULL_BYTES",
            "short summary",
        )))
        .tool(Arc::new(DeferringTool))
        .build();

    let outcome = agent.run("go").await.unwrap();
    assert_eq!(outcome.stop_reason.as_deref(), Some("suspended"));

    let payloads = persisted_payloads(&agent);
    assert!(
        payloads.iter().any(|p| p.text() == "short summary"),
        "no store: the summary stays, full bytes never enter the log: {payloads:?}"
    );
    assert!(
        !payloads.iter().any(|p| p.text().contains("THE_FULL_BYTES")),
        "full bytes must not leak inline on suspension"
    );
}

#[tokio::test]
async fn summaries_and_failures_respect_the_inline_budget() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        multi_tool_response(vec![
            ("c1", "summary_tool", serde_json::json!({})),
            ("c2", "failer", serde_json::json!({})),
        ]),
        text_response("done"),
    ]));

    struct BigFailer;
    #[async_trait]
    impl Tool for BigFailer {
        fn name(&self) -> &str {
            "failer"
        }
        fn description(&self) -> &str {
            "fails loudly"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({ "type": "object" })
        }
        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolContext,
        ) -> anyhow::Result<ToolResult> {
            Ok(ToolResult::error("E".repeat(5000)))
        }
    }

    let huge_summary = "S".repeat(5000);
    let mut agent = Session::builder(provider, "t", "s")
        .model("test")
        .tool(Arc::new(SummaryTool::new("full", &huge_summary)))
        .tool(Arc::new(BigFailer))
        .auto_spill_over(128)
        .build();

    agent.run("go").await.unwrap();

    for payload in persisted_payloads(&agent) {
        let text = payload.text();
        assert!(
            text.len() <= 128,
            "the budget is a hard byte ceiling, got {} bytes",
            text.len()
        );
        assert!(text.contains("[truncated from 5000 bytes]"), "{text}");
    }
}
