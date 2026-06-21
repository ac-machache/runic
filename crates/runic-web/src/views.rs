//! Render helpers: chat transcript, the Events-tab Run/Turn tree, and the
//! State tab. Pure view functions returning `AnyView`.

use leptos::prelude::*;
use serde_json::Value;

use crate::model::{Item, RunCluster, ToolView, TurnCluster};
use crate::util::{
    clean_tool_name, content_blocks, md_to_html, render_block_summary, short_id, truncate,
};

// ── chat transcript ──────────────────────────────────────────────────────

pub fn render_item(item: Item) -> AnyView {
    match item {
        Item::User(text) => view! {
            <div class="msg-user"><div class="bubble-user">{text}</div></div>
        }.into_any(),
        Item::Assistant(text) => view! {
            <div class="msg-assistant">
                <img class="avatar" src="favicon.png" alt="runic" />
                <div class="assistant-body"><div class="prose" inner_html=md_to_html(&text)></div></div>
            </div>
        }.into_any(),
        Item::Thinking(text) => view! {
            <div class="msg-assistant">
                <img class="avatar" src="favicon.png" alt="runic" />
                <div class="assistant-body">
                    <details class="thinking"><summary>"thinking"</summary><div class="thinking-body">{text}</div></details>
                </div>
            </div>
        }.into_any(),
        Item::Warning(text) => view! {
            <div class="warn"><span class="ic">"⚠"</span><span class="tx">{text}</span></div>
        }.into_any(),
        Item::Tool(t) => render_tool_card(t),
    }
}

fn render_tool_card(t: ToolView) -> AnyView {
    let dot_cls = format!("tool-dot {}", t.status);
    let status_cls = format!("tool-status {}", t.status);
    let dur = if t.duration_ms > 0 {
        format!("· {}ms", t.duration_ms)
    } else {
        String::new()
    };
    let label = clean_tool_name(&t.name);
    let has_input = !t.input.is_empty();
    let input = t.input.clone();
    let has_result = !t.result.is_empty();
    let is_err = t.status == "error";
    let result = t.result.clone();
    let res_cls = if is_err { "jsonpre error" } else { "jsonpre" };
    let sources = t.sources.clone();
    view! {
        <div class="tool">
            <div class="tool-head">
                <span class=dot_cls></span>
                <span class="tool-name">{label}</span>
                <span class=status_cls>{t.status.clone()}</span>
                <span class="tool-dur">{dur}</span>
            </div>
            {has_input.then(|| view! {
                <details class="tool-sec"><summary>"args"</summary><pre class="jsonpre">{input}</pre></details>
            })}
            {has_result.then(move || view! {
                <details class="tool-sec">
                    <summary>"result"</summary>
                    <pre class=res_cls>{result}</pre>
                    {(!sources.is_empty()).then(|| view! {
                        <div class="sources">
                            {sources.iter().map(|s| {
                                let url = s.url.clone();
                                let title = if s.title.is_empty() { s.url.clone() } else { s.title.clone() };
                                view! { <a class="chip" href=url target="_blank">{title}<span class="lk">"🔗"</span></a> }
                            }).collect_view()}
                        </div>
                    })}
                </details>
            })}
        </div>
    }.into_any()
}

// ── events tab: run/turn cluster rendering ───────────────────────────────

/// Render one RUN (top level) — a user message + its answer — expanding to
/// the model turns that happened in between.
pub fn render_run(idx: usize, total: usize, r: RunCluster, show_thinking: bool) -> AnyView {
    let dot_cls = if r.running {
        "run-dot running"
    } else if r.errored {
        "run-dot error"
    } else {
        "run-dot"
    };
    let has_prompt = !r.prompt.is_empty();
    let prompt = r.prompt.clone();
    let label = if has_prompt {
        truncate(r.prompt.lines().next().unwrap_or(""), 46)
    } else if r.id.is_empty() {
        format!("run {}", idx + 1)
    } else {
        format!("run · {}", short_id(&r.id))
    };
    let n = r.turns.len();
    let turns_label = format!("{n} turn{}", if n == 1 { "" } else { "s" });
    let time = if r.running { "live" } else { "done" };
    let stop = r.stop_reason.clone();
    let usage = r.usage;
    let open = r.running || idx + 1 == total; // newest / live run expanded
    let turn_views = r
        .turns
        .into_iter()
        .enumerate()
        .map(|(i, t)| render_turn(i, t, show_thinking))
        .collect_view();
    view! {
        <details class="run" open=open>
            <summary>
                <span class=dot_cls></span>
                <span class="run-prompt-preview">{label}</span>
                <span class="run-meta">
                    <span class="run-time">{time}</span>
                    {stop.map(|s| view! { <span class="mono">{s}</span> })}
                    <span>{turns_label}</span>
                    {usage.map(|(i, o)| view! { <span class="mono">{format!("↑{i} ↓{o}")}</span> })}
                </span>
            </summary>
            <div class="run-body">
                {has_prompt.then(|| view! {
                    <div class="blk blk-user"><span class="blk-tag tag-user">"user"</span><span class="blk-tx">{prompt}</span></div>
                })}
                {turn_views}
            </div>
        </details>
    }.into_any()
}

/// Render one model TURN (nested inside a run): assistant text, optional
/// thinking, and the tool calls (args + result) for that step.
fn render_turn(idx: usize, t: TurnCluster, show_thinking: bool) -> AnyView {
    let running = !t.closed;
    let calls = t.tool_calls.max(t.tools.len() as u32);
    let has_text = !t.text.is_empty();
    let text = t.text.clone();
    let show_think = show_thinking && !t.thinking.is_empty();
    let thinking = t.thinking.clone();
    let tool_views = t.tools.iter().map(render_turn_tool).collect_view();
    let foot = t.closed.then(|| {
        format!(
            "stop_reason: {} · tool_calls: {}",
            t.stop_reason.clone().unwrap_or_else(|| "—".into()),
            calls
        )
    });
    view! {
        <details class="turn" open=true>
            <summary>
                <span class="turn-name">{format!("Turn {}", idx + 1)}</span>
                {if running {
                    view! { <span class="turn-meta running"><span class="rdot"></span>"streaming"</span> }.into_any()
                } else {
                    view! { <span class="turn-meta">{format!("{calls} call(s)")}</span> }.into_any()
                }}
            </summary>
            <div class="turn-body">
                {show_think.then(|| view! {
                    <div class="blk blk-think"><span class="blk-tag tag-think">"thinking"</span><span class="blk-tx">{thinking}</span></div>
                })}
                {has_text.then(|| view! {
                    <div class="blk blk-ai"><span class="blk-tag tag-ai">"AI"</span><span class="blk-tx">{text}</span></div>
                })}
                {tool_views}
                {foot.map(|f| view! { <div class="turn-foot">{f}</div> })}
            </div>
        </details>
    }.into_any()
}

fn render_turn_tool(t: &ToolView) -> AnyView {
    let dot_cls = format!("dot {}", t.status);
    let pill_cls = format!("status-pill {}", t.status);
    let status = t.status.clone();
    let label = clean_tool_name(&t.name);
    let dur = if t.duration_ms > 0 {
        format!("{}ms", t.duration_ms)
    } else {
        String::new()
    };
    let mut body = String::new();
    if !t.input.is_empty() {
        body.push_str(&t.input);
    }
    if !t.result.is_empty() {
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str("→ ");
        body.push_str(&t.result);
    }
    let has_body = !body.is_empty();
    view! {
        <div class="blk blk-tool">
            <div class="blk-head">
                <span class="blk-tag tag-tool">"tool"</span>
                <span class=dot_cls></span>
                <span class="nm">{label}</span>
                <span class=pill_cls>{status}</span>
                <span class="dur">{dur}</span>
            </div>
            {has_body.then(|| view! {
                <details class="blk-tool-sec"><summary>"args · result"</summary><pre class="jsonpre">{body}</pre></details>
            })}
        </div>
    }.into_any()
}

// ── state tab ────────────────────────────────────────────────────────────

/// Render the agent-state inspector. The new `/state` endpoint exposes
/// `system_prompt`, `messages`, and counts (it no longer ships an assembled vs
/// base prompt or the tool schemas).
pub fn render_state(s: &Value, refresh: impl Fn() + Copy + 'static) -> AnyView {
    let busy = s.get("busy").and_then(|v| v.as_bool()).unwrap_or(false);
    let prompt = s
        .get("system_prompt")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let event_count = s.get("event_count").and_then(|v| v.as_u64()).unwrap_or(0);
    let run_count = s
        .get("run_count")
        .and_then(|v| v.as_u64())
        .map(|r| r.to_string())
        .unwrap_or_else(|| "—".into());
    let messages = s
        .get("messages")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let msg_count = messages.len();

    let msg_views = messages.into_iter().map(|m| {
        let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("?").to_string();
        let body = content_blocks(&m).iter().map(render_block_summary).collect::<Vec<_>>().join("\n");
        let preview = truncate(body.lines().next().unwrap_or(""), 80);
        let chip_cls = format!("role-chip {}", role.to_lowercase());
        view! {
            <details class="state-msg">
                <summary><span class=chip_cls>{role}</span><span class="preview">{preview}</span></summary>
                <div class="state-msg-body">{body}</div>
            </details>
        }
    }).collect_view();

    view! {
        <div>
            <div class="state-counts">
                <span class="count"><strong>{run_count}</strong>" runs"</span>
                <span class="count-sep">"·"</span>
                <span class="count"><strong>{event_count.to_string()}</strong>" events"</span>
                <span class="count-sep">"·"</span>
                <span class="count"><strong>{msg_count.to_string()}</strong>" messages"</span>
                {busy.then(|| view! { <span class="busy-badge"><span class="d"></span>"busy"</span> })}
                <button class="copy-btn" title="Refresh" on:click=move |_| refresh()>"⟳"</button>
            </div>

            <div class="state-body">
                <div>
                    <div class="state-section-head">
                        <span class="state-section-cap">"System prompt"</span>
                    </div>
                    <div class="prompt-view">{prompt}</div>
                </div>

                <div>
                    <div class="state-section-head">
                        <span class="state-section-cap">"Messages"</span>
                        <span class="state-count">{format!("{msg_count} · as sent to model")}</span>
                    </div>
                    <div class="msg-list">{msg_views}</div>
                </div>
            </div>
        </div>
    }.into_any()
}
