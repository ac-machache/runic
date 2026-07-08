use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::Utc;
use runic::compose::{Compose, Composer, Delegation, Memory, Skills, Toolset};
use runic_agent::Agent;
use runic_memory::{Target, memory};
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_skills::SkillSet;
use runic_state::{AgentState, SessionEvent, ThreadStats};
use runic_substrate::{MemorySessionStore, SessionStore};
use runic_types::{ContentBlock, Message, StopReason, TokenUsage};

struct NoopProvider;

#[async_trait]
impl Provider for NoopProvider {
    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        Ok(CompletionResponse {
            content: vec![ContentBlock::Text {
                text: "ok".into(),
                provider_metadata: None,
            }],
            stop_reason: StopReason::EndTurn,
            tool_calls: vec![],
            usage: TokenUsage::default(),
        })
    }
}

async fn write_skill(root: &std::path::Path, name: &str) {
    let dir = root.join(name);
    tokio::fs::create_dir_all(&dir).await.unwrap();
    tokio::fs::write(
        dir.join("SKILL.md"),
        format!(
            "---\nname: {name}\ndescription: does {name} things in several steps\n---\n{}",
            "step ".repeat(200)
        ),
    )
    .await
    .unwrap();
}

async fn write_agent(root: &std::path::Path, name: &str) {
    tokio::fs::write(
        root.join(format!("{name}.md")),
        format!(
            "---\nname: {name}\ndescription: handles {name} work\n---\nYou are {name}. {}",
            "detail ".repeat(120)
        ),
    )
    .await
    .unwrap();
}

struct Fixture {
    skill_dir: tempfile::TempDir,
    agent_dir: tempfile::TempDir,
    memory_dir: tempfile::TempDir,
}

async fn fixture() -> Fixture {
    let skill_dir = tempfile::tempdir().unwrap();
    let agent_dir = tempfile::tempdir().unwrap();
    let memory_dir = tempfile::tempdir().unwrap();
    for name in ["review", "research", "deploy", "triage", "summarize"] {
        write_skill(skill_dir.path(), name).await;
    }
    for name in ["scout", "coder", "critic"] {
        write_agent(agent_dir.path(), name).await;
    }
    let cfg = memory(memory_dir.path()).init().scope_per_tenant();
    let store = cfg.store("bench-tenant").await;
    for i in 0..20 {
        store
            .add(Target::Memory, &format!("durable project fact number {i}"))
            .await
            .unwrap();
        store
            .add(Target::User, &format!("user preference number {i}"))
            .await
            .unwrap();
    }
    Fixture {
        skill_dir,
        agent_dir,
        memory_dir,
    }
}

fn assembly(fx: &Fixture, skills: Arc<SkillSet>) -> Composer {
    Agent::compose(Arc::new(NoopProvider), "bench")
        .instructions("you are the bench agent ".repeat(50))
        .with(Memory(
            memory(fx.memory_dir.path()).init().scope_per_tenant(),
        ))
        .with(Skills(skills))
        .with(Delegation::new(runic_subagent::subagents(
            fx.agent_dir.path(),
        )))
        .with(Toolset(runic_tools::tools()))
}

async fn timed<F, Fut>(label: &str, iters: u32, mut f: F) -> Duration
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    for _ in 0..5 {
        f().await;
    }
    let start = Instant::now();
    for _ in 0..iters {
        f().await;
    }
    let avg = start.elapsed() / iters;
    println!("{label:55} {avg:>12.2?}  per request");
    avg
}

#[tokio::test]
#[ignore]
async fn cost_of_pure_stateless_rebuild() {
    let fx = fixture().await;

    let skills = Arc::new(SkillSet::load_dir("", fx.skill_dir.path()).await);

    println!();
    println!("== what a request pays, by design ==");

    timed(
        "A. everything from scratch (skills+roster+assemble)",
        50,
        || {
            let fx = &fx;
            async move {
                let skills = Arc::new(SkillSet::load_dir("", fx.skill_dir.path()).await);
                let a = assembly(fx, skills);
                let agent = a.build("bench-tenant", "t1").await;
                std::hint::black_box(agent);
            }
        },
    )
    .await;

    let shared = assembly(&fx, skills.clone());
    timed(
        "B. boot resources shared, assemble per request",
        200,
        || {
            let a = &shared;
            async move {
                let agent = a.build("bench-tenant", "t1").await;
                std::hint::black_box(agent);
            }
        },
    )
    .await;

    let prebuilt_store = memory(fx.memory_dir.path())
        .init()
        .scope_per_tenant()
        .store("bench-tenant")
        .await;
    timed("C. memory snapshot read alone (per request)", 200, || {
        let store = prebuilt_store.clone();
        async move {
            let snap = store.snapshot().await.unwrap();
            std::hint::black_box(snap);
        }
    })
    .await;
}

fn fat_message(i: usize) -> SessionEvent {
    SessionEvent::Message {
        run_id: format!("r-{}", i / 4),
        msg: Message::user_with_blocks(vec![ContentBlock::ToolResult {
            tool_use_id: format!("t{i}"),
            tool_name: "web_fetch".into(),
            content: format!("tool output {i} {}", "payload ".repeat(256)),
            is_error: false,
        }]),
        at: Utc::now(),
    }
}

#[tokio::test]
#[ignore]
async fn cost_of_state_hydration_per_request() {
    let store = Arc::new(MemorySessionStore::new());

    let mut events: Vec<SessionEvent> = Vec::new();
    for i in 0..400 {
        events.push(fat_message(i));
    }
    events.push(SessionEvent::StateSnapshot {
        run_id: "r-99".into(),
        messages: (0..10)
            .map(|i| Message::assistant(format!("kept {i}")))
            .collect(),
        system_prompt: "sys".into(),
        reason: "compaction".into(),
        stats: Some(ThreadStats::default()),
        open_tasks: Some(vec![]),
        data: None,
        at: Utc::now(),
    });
    for i in 0..30 {
        events.push(fat_message(1000 + i));
    }
    store.append_batch("t", "s", &events).await.unwrap();

    println!();
    println!("== state hydration (memory store; add ~0.5-2ms per query on Postgres) ==");

    timed(
        "read_tail + fold (30-msg working set, 431-event log)",
        300,
        || {
            let store = store.clone();
            async move {
                let tail = store.read_tail("t", "s").await.unwrap();
                let mut state = AgentState::new("t", "s", "sys");
                for entry in tail {
                    state.fold_event(entry.event);
                }
                std::hint::black_box(state);
            }
        },
    )
    .await;

    timed(
        "full-log replay (431 events) — the OLD cold build",
        300,
        || {
            let store = store.clone();
            async move {
                let all = store.read("t", "s").await.unwrap();
                let mut state = AgentState::new("t", "s", "sys");
                for entry in all {
                    state.fold_event(entry.event);
                }
                std::hint::black_box(state);
            }
        },
    )
    .await;
}
