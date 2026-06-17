//! runic dev console — a Leptos CSR app for driving `runic serve`.
//!
//! Layout: a collapsible sidebar (connection + threads), then a chat pane and
//! an inspector pane split 50/50 by a draggable splitter. The inspector has
//! two tabs: **Events** (the run/turn-clustered activity tree — NOT a raw
//! token firehose) and **State** (assembled prompt, tools, messages). Talks to
//! the server over HTTP + SSE; events are parsed leniently as JSON so the UI
//! never hard-couples to the server's internal `WireEvent` type.
//!
//! Visual design ported from Claude Design (warm "paper" aesthetic, light +
//! dark themes via a `.dark` class on the root).

use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::Value;

mod api;
use api::ApiClient;

/// The actively-streaming tail. Tokens append here (one reactive text node)
/// instead of mutating the `items` list, so per-token cost is O(1) DOM. On a
/// boundary (a non-text event) or run end it flushes into `items` as a
/// finalized, markdown-rendered message.
#[derive(Clone, Default, PartialEq)]
struct LiveBuf {
    kind: LiveKind,
    text: String,
}

#[derive(Clone, Copy, Default, PartialEq)]
enum LiveKind {
    #[default]
    None,
    Text,
    Thinking,
}

/// One rendered entry in the chat transcript.
#[derive(Clone, Debug)]
enum Item {
    User(String),
    Assistant(String),
    Thinking(String),
    Tool(ToolView),
    Warning(String),
}

#[derive(Clone, Debug, Default)]
struct ToolView {
    id: String,
    name: String,
    input: String,
    status: String,
    result: String,
    duration_ms: u64,
    sources: Vec<Source>,
}

#[derive(Clone, Debug)]
struct Source {
    title: String,
    url: String,
}

/// A HITL tool waiting for the operator's decision.
#[derive(Clone)]
struct PendingApproval {
    call_id: String,
    tool_name: String,
    summary: String,
    current_input: serde_json::Value,
    fields: Vec<(String, String)>,
}

// ── Events tab clustering (Run → Turn → details) ─────────────────────────

/// A run = one user message and the agent's answer to it. Holds the model
/// turns that happened in between.
#[derive(Clone, Default)]
struct RunCluster {
    id: String,
    prompt: String,
    running: bool,
    ended: bool,
    errored: bool,
    turns: Vec<TurnCluster>,
    stop_reason: Option<String>,
    usage: Option<(u64, u64)>,
}

#[derive(Clone, Default)]
struct TurnCluster {
    text: String,
    thinking: String,
    tools: Vec<ToolView>,
    stop_reason: Option<String>,
    tool_calls: u32,
    closed: bool,
}

fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(App);
}

#[component]
fn App() -> impl IntoView {
    // ── connection / identity ──────────────────────────────────────────
    let api_base = RwSignal::new("http://127.0.0.1:8920".to_string());
    let tenant = RwSignal::new("default".to_string());

    // ── thread + transcript state ──────────────────────────────────────
    let threads = RwSignal::new(Vec::<String>::new());
    let current = RwSignal::new(None::<String>);
    let items = RwSignal::new(Vec::<Item>::new());
    let live = RwSignal::new(LiveBuf::default());
    let events = RwSignal::new(Vec::<Value>::new());
    let usage = RwSignal::new(None::<(u64, u64)>);
    let input = RwSignal::new(String::new());
    let streaming = RwSignal::new(false);
    let output_schema = RwSignal::new(String::new());
    let context_json = RwSignal::new(String::new());
    let pending = RwSignal::new(None::<PendingApproval>);
    let has_pending = Memo::new(move |_| pending.get().is_some());

    let abort = RwSignal::new(None::<web_sys::AbortController>);
    let inspect_tab = RwSignal::new("events");
    let state_json = RwSignal::new(None::<Value>);

    // ── chrome / UI state ──────────────────────────────────────────────
    let dark = RwSignal::new(false);
    let collapsed = RwSignal::new(false);
    let config_open = RwSignal::new(false);
    let split = RwSignal::new(50.0_f64); // chat % of the main area
    let dragging = RwSignal::new(false);
    let show_thinking = RwSignal::new(false);
    let prompt_assembled = RwSignal::new(true); // state tab: assembled vs base
    let main_ref = NodeRef::<leptos::html::Div>::new();
    let splitter_ref = NodeRef::<leptos::html::Div>::new();

    // Auto-scroll the transcript as content arrives.
    let transcript_ref = NodeRef::<leptos::html::Div>::new();
    Effect::new(move |_| {
        items.track();
        live.track();
        if let Some(el) = transcript_ref.get() {
            el.set_scroll_top(el.scroll_height());
        }
    });

    let client = move || ApiClient::new(api_base.get_untracked(), tenant.get_untracked());

    let refresh_threads = move || {
        let c = client();
        spawn_local(async move {
            match c.list_threads().await {
                Ok(ids) => threads.set(ids),
                Err(e) => leptos::logging::warn!("list_threads failed: {e}"),
            }
        });
    };

    let load_thread = move |id: String| {
        current.set(Some(id.clone()));
        items.set(Vec::new());
        live.set(LiveBuf::default());
        events.set(Vec::new());
        state_json.set(None);
        let c = client();
        spawn_local(async move {
            match c.thread_events(&id).await {
                Ok(evs) => {
                    items.set(items_from_events(&evs));
                    events.set(evs);
                }
                Err(e) => leptos::logging::warn!("load history failed: {e}"),
            }
        });
    };

    let new_thread = move || {
        let c = client();
        spawn_local(async move {
            match c.create_thread(None).await {
                Ok(id) => {
                    current.set(Some(id.clone()));
                    items.set(Vec::new());
                    live.set(LiveBuf::default());
                    events.set(Vec::new());
                    state_json.set(None);
                    match c.list_threads().await {
                        Ok(ids) => threads.set(ids),
                        Err(_) => threads.update(|t| if !t.contains(&id) { t.push(id.clone()) }),
                    }
                }
                Err(e) => leptos::logging::warn!("create_thread failed: {e}"),
            }
        });
    };

    let send = move || {
        let text = input.get_untracked();
        if text.trim().is_empty() || streaming.get_untracked() {
            return;
        }
        let c = client();
        let thread_id = match current.get_untracked() {
            Some(id) => id,
            None => return,
        };
        let schema_text = output_schema.get_untracked();
        let schema_val: Option<Value> = if schema_text.trim().is_empty() {
            None
        } else {
            match serde_json::from_str::<Value>(&schema_text) {
                Ok(v) => Some(v),
                Err(e) => {
                    items.update(|its| its.push(Item::Warning(format!("invalid output schema JSON: {e}"))));
                    return;
                }
            }
        };
        let context_text = context_json.get_untracked();
        let context_val: Option<Value> = if context_text.trim().is_empty() {
            None
        } else {
            match serde_json::from_str::<Value>(&context_text) {
                Ok(v) => Some(v),
                Err(e) => {
                    items.update(|its| its.push(Item::Warning(format!("invalid context JSON: {e}"))));
                    return;
                }
            }
        };

        input.set(String::new());
        items.update(|its| its.push(Item::User(text.clone())));
        // The live SSE stream carries only agent deltas (no run_start / user
        // message), so mark the run boundary + prompt ourselves for the
        // Events clusterer.
        events.update(|e| e.push(serde_json::json!({ "type": "run_begin", "prompt": text })));
        streaming.set(true);

        let controller = web_sys::AbortController::new().ok();
        let signal = controller.as_ref().map(|c| c.signal());
        abort.set(controller);

        spawn_local(async move {
            let on_event = move |ev: Value| {
                events.update(|e| e.push(ev.clone()));
                if let Some(("usage", (i, o))) = usage_of(&ev) {
                    usage.set(Some((i, o)));
                }
                let kind = ev.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match kind {
                    "assistant_text_delta" => {
                        let t = ev.get("text").and_then(|v| v.as_str()).unwrap_or("");
                        append_live(live, items, LiveKind::Text, t);
                    }
                    "assistant_thinking_delta" => {
                        let t = ev.get("text").and_then(|v| v.as_str()).unwrap_or("");
                        append_live(live, items, LiveKind::Thinking, t);
                    }
                    "approval_required" => {
                        flush_live(live, items);
                        if let Some(p) = parse_pending(&ev) {
                            pending.set(Some(p));
                        }
                    }
                    // structured_output is intentionally not surfaced.
                    _ => {
                        flush_live(live, items);
                        items.update(|its| apply_event(its, &ev));
                    }
                }
            };
            let result = c.stream_run(&thread_id, &text, schema_val, context_val, signal.as_ref(), on_event).await;
            flush_live(live, items);
            if let Err(e) = result {
                if !e.to_lowercase().contains("abort") {
                    items.update(|its| its.push(Item::Warning(format!("stream error: {e}"))));
                } else {
                    items.update(|its| its.push(Item::Warning("— stopped —".into())));
                }
            }
            streaming.set(false);
            abort.set(None);
            pending.set(None);
        });
    };

    let stop = move || {
        if let Some(c) = abort.get_untracked() {
            c.abort();
        }
    };

    let fetch_state = move || {
        let Some(id) = current.get_untracked() else { return };
        let c = client();
        spawn_local(async move {
            match c.thread_state(&id).await {
                Ok(v) => state_json.set(Some(v)),
                Err(e) => leptos::logging::warn!("state fetch failed: {e}"),
            }
        });
    };

    // ── splitter drag (pointer capture keeps events on the handle) ──────
    let on_split_down = move |e: web_sys::PointerEvent| {
        e.prevent_default();
        dragging.set(true);
        if let Some(el) = splitter_ref.get() {
            let _ = el.set_pointer_capture(e.pointer_id());
        }
    };
    let on_split_move = move |e: web_sys::PointerEvent| {
        if !dragging.get_untracked() {
            return;
        }
        if let Some(m) = main_ref.get_untracked() {
            let rect = m.get_bounding_client_rect();
            if rect.width() > 0.0 {
                let pct = (e.client_x() as f64 - rect.left()) / rect.width() * 100.0;
                split.set(pct.clamp(28.0, 72.0));
            }
        }
    };
    let on_split_up = move |_e: web_sys::PointerEvent| dragging.set(false);

    refresh_threads();

    view! {
        <div class="app" class:dark=move || dark.get()>

            // ░░ SIDEBAR ░░
            <aside class="sidebar" style:width=move || if collapsed.get() { "0px".to_string() } else { "264px".to_string() }>
                <div class="sidebar-inner">
                    <div class="brand">
                        <div class="brand-l">
                            <span class="brand-mark">"⟡"</span>
                            <div>
                                <div class="brand-name">"runic"</div>
                                <div class="brand-sub">"dev console"</div>
                            </div>
                        </div>
                        <button class="collapse-btn" title="Collapse sidebar" on:click=move |_| collapsed.set(true)>"«"</button>
                    </div>

                    <div class="conn">
                        <div class="section-cap conn-cap">"Connection"</div>
                        <label class="conn-label">"server URL"</label>
                        <input class="conn-input" spellcheck="false" prop:value=move || api_base.get()
                            on:input=move |e| api_base.set(event_target_value(&e)) />
                        <div class="conn-label row"><span>"tenant"</span><span class="conn-hint">"X-Runic-Tenant"</span></div>
                        <input class="conn-input" spellcheck="false" prop:value=move || tenant.get()
                            on:input=move |e| tenant.set(event_target_value(&e)) />
                        <div class="conn-status"><span class="status-dot"></span><span>"connected"</span></div>
                    </div>

                    <div class="threads-head">
                        <span class="section-cap">"Threads"</span>
                        <button class="icon-btn" title="Refresh" on:click=move |_| refresh_threads()>"⟳"</button>
                    </div>
                    <div class="newthread-wrap">
                        <button class="newthread" on:click=move |_| new_thread()><span>"＋"</span>"New thread"</button>
                    </div>

                    <div class="thread-list">
                        {move || threads.get().into_iter().map(|id| {
                            let id_active = id.clone();
                            let id_click = id.clone();
                            let label = short_id(&id);
                            view! {
                                <div class="thread"
                                    class:active=move || current.get().as_deref() == Some(id_active.as_str())
                                    on:click=move |_| load_thread(id_click.clone())>
                                    <span class="thread-accent"></span>
                                    <div class="thread-row1">
                                        <span class="thread-id">{label}</span>
                                    </div>
                                    <div class="thread-title untitled">"untitled"</div>
                                </div>
                            }
                        }).collect_view()}
                    </div>

                    <div class="theme-bar">
                        <span class="theme-label">{move || if dark.get() { "Warm dark" } else { "Paper light" }}</span>
                        <button class="theme-btn" on:click=move |_| dark.update(|d| *d = !*d)>
                            {move || if dark.get() { "☀ Theme".to_string() } else { "☾ Theme".to_string() }}
                        </button>
                    </div>
                </div>
            </aside>

            // ░░ MAIN: chat | splitter | inspector ░░
            <div class="main" node_ref=main_ref>

                <section class="chat" style:flex=move || format!("1 1 {}%", split.get())>
                    <div class="topbar">
                        {move || collapsed.get().then(|| view! {
                            <button class="rail-btn" title="Open sidebar" on:click=move |_| collapsed.set(false)>"»"</button>
                        })}
                        <span class="topbar-title">"Chat"</span>
                        {move || current.get().map(|id| view! { <span class="thread-chip">{short_id(&id)}</span> })}
                        {move || streaming.get().then(|| view! {
                            <span class="stream-ind"><span class="stream-dot"></span>"streaming"</span>
                        })}
                    </div>

                    <div class="transcript" node_ref=transcript_ref>
                        <div class="transcript-inner">
                            {move || (items.get().is_empty() && live.get().text.is_empty()).then(|| {
                                let msg = if current.get().is_some() {
                                    "Send a message to start the conversation."
                                } else {
                                    "Create or select a thread to begin."
                                };
                                view! { <div class="empty">{msg}</div> }
                            })}
                            {move || items.get().into_iter().map(render_item).collect_view()}
                            {move || {
                                let lb = live.get();
                                if lb.text.is_empty() {
                                    ().into_any()
                                } else if matches!(lb.kind, LiveKind::Thinking) {
                                    view! {
                                        <div class="msg-assistant">
                                            <div class="avatar">"⟡"</div>
                                            <div class="assistant-body"><div class="thinking-body">{lb.text}</div></div>
                                        </div>
                                    }.into_any()
                                } else {
                                    view! {
                                        <div class="msg-assistant">
                                            <div class="avatar">"⟡"</div>
                                            <div class="assistant-body">
                                                <div class="prose"><p style="margin:0">{lb.text}<span class="caret"></span></p></div>
                                            </div>
                                        </div>
                                    }.into_any()
                                }
                            }}
                        </div>
                    </div>

                    // approval card (HITL)
                    {move || has_pending.get().then(|| {
                        let p = pending.get_untracked().expect("has_pending implies Some");
                        let field_views = p.fields.iter().enumerate().map(|(i, (name, val))| {
                            view! {
                                <label class="apv-label">{name.clone()}</label>
                                <input class="apv-input" value=val.clone()
                                    on:input=move |e| pending.update(|opt| {
                                        if let Some(p) = opt
                                            && let Some(f) = p.fields.get_mut(i) { f.1 = event_target_value(&e); }
                                    }) />
                            }
                        }).collect_view();
                        let approve = move |_| {
                            let Some(p) = pending.get_untracked() else { return };
                            let mut fi = p.current_input.clone();
                            if !fi.is_object() { fi = serde_json::json!({}); }
                            if let Some(obj) = fi.as_object_mut() {
                                for (name, val) in &p.fields { obj.insert(name.clone(), Value::String(val.clone())); }
                            }
                            let decision = serde_json::json!({ "decision": "submit", "final_input": fi });
                            let c = client();
                            let thread = current.get_untracked().unwrap_or_default();
                            let call_id = p.call_id.clone();
                            pending.set(None);
                            spawn_local(async move { let _ = c.submit_approval(&thread, &call_id, decision).await; });
                        };
                        let cancel = move |_| {
                            let Some(p) = pending.get_untracked() else { return };
                            let decision = serde_json::json!({ "decision": "cancel", "reason": "declined by operator" });
                            let c = client();
                            let thread = current.get_untracked().unwrap_or_default();
                            pending.set(None);
                            spawn_local(async move { let _ = c.submit_approval(&thread, &p.call_id, decision).await; });
                        };
                        view! {
                            <div class="composer">
                                <div class="composer-inner">
                                    <div class="approval">
                                        <div class="apv-head">
                                            <span class="ic">"⏸"</span>
                                            <span class="apv-name">{p.tool_name.clone()}</span>
                                            <span class="apv-badge">"approval required"</span>
                                        </div>
                                        <div class="apv-body">
                                            <div class="apv-summary">{p.summary.clone()}</div>
                                            {field_views}
                                            <div class="apv-actions">
                                                <button class="apv-submit" on:click=approve>"Submit"</button>
                                                <button class="apv-cancel" on:click=cancel>"Cancel"</button>
                                            </div>
                                        </div>
                                    </div>
                                </div>
                            </div>
                        }
                    })}

                    // composer
                    <div class="composer">
                        <div class="composer-inner">
                            {move || config_open.get().then(|| view! {
                                <div class="config-pop">
                                    <div class="config-head">
                                        <div class="config-head-l">
                                            <span>"⚙"</span>
                                            <span class="config-title">"Configurable"</span>
                                            <span class="config-sub">"per-run context"</span>
                                        </div>
                                        <button class="config-x" on:click=move |_| config_open.set(false)>"✕"</button>
                                    </div>
                                    <div class="config-body">
                                        <div class="config-row">
                                            <label class="section-cap">"context"</label>
                                            <span class="conn-hint">"open map · sent verbatim"</span>
                                            {move || {
                                                let t = context_json.get();
                                                if t.trim().is_empty() {
                                                    ().into_any()
                                                } else if serde_json::from_str::<Value>(&t).is_ok() {
                                                    view! { <span class="config-valid ok">"● valid"</span> }.into_any()
                                                } else {
                                                    view! { <span class="config-valid bad">"● invalid"</span> }.into_any()
                                                }
                                            }}
                                        </div>
                                        <textarea class="config-ta" spellcheck="false"
                                            placeholder=r#"{ "user_id": "u1", "provider": "sonnet", "allow_web_search": true }"#
                                            prop:value=move || context_json.get()
                                            on:input=move |e| context_json.set(event_target_value(&e))></textarea>
                                        <details class="config-schema">
                                            <summary>"output schema (optional JSON)"</summary>
                                            <textarea class="config-schema-ta" spellcheck="false"
                                                placeholder=r#"{ "type": "object", "properties": ... }"#
                                                prop:value=move || output_schema.get()
                                                on:input=move |e| output_schema.set(event_target_value(&e))></textarea>
                                        </details>
                                    </div>
                                </div>
                            })}

                            <div class="input-row">
                                <textarea class="composer-input" spellcheck="false"
                                    rows=move || input.get().lines().count().clamp(1, 6).to_string()
                                    prop:value=move || input.get()
                                    on:input=move |e| input.set(event_target_value(&e))
                                    on:keydown=move |e| {
                                        if e.key() == "Enter" && !e.shift_key() {
                                            e.prevent_default();
                                            send();
                                        }
                                    }
                                    prop:disabled=move || current.get().is_none() || streaming.get()
                                    placeholder=move || if current.get().is_some() {
                                        "Message the agent…  (Enter to send)".to_string()
                                    } else {
                                        "Create or pick a thread first".to_string()
                                    }></textarea>
                                <button class="gear-btn" title="Configurable" on:click=move |_| config_open.update(|c| *c = !*c)>
                                    "⚙"
                                    {move || (!context_json.get().trim().is_empty()).then(|| view! { <span class="gear-dot"></span> })}
                                </button>
                                {move || if streaming.get() {
                                    view! { <button class="send-btn stop" on:click=move |_| stop()>"◼ Stop"</button> }.into_any()
                                } else {
                                    view! {
                                        <button class="send-btn" on:click=move |_| send()
                                            prop:disabled=move || current.get().is_none()>"Send ↵"</button>
                                    }.into_any()
                                }}
                            </div>
                            <div class="composer-hint">"Enter to send · Shift+Enter for newline"</div>
                        </div>
                    </div>
                </section>

                // splitter
                <div class="splitter" class:dragging=move || dragging.get() node_ref=splitter_ref
                    on:pointerdown=on_split_down on:pointermove=on_split_move on:pointerup=on_split_up
                    on:dblclick=move |_| split.set(50.0)
                    title="Drag to resize · double-click to reset"></div>

                // ░░ INSPECTOR ░░
                <section class="inspector" style:flex=move || format!("1 1 {}%", 100.0 - split.get())>
                    <div class="topbar">
                        <span class="topbar-title dim">"Inspector"</span>
                        <div class="tabs">
                            <button class="tab" class:on=move || inspect_tab.get() == "events"
                                on:click=move |_| inspect_tab.set("events")>"Events"</button>
                            <button class="tab" class:on=move || inspect_tab.get() == "state"
                                on:click=move |_| { inspect_tab.set("state"); fetch_state(); }>"State"</button>
                        </div>
                    </div>

                    <div class="tab-body">
                        // EVENTS
                        {move || (inspect_tab.get() == "events").then(|| {
                            let st = show_thinking.get();
                            // Top level = RUN (one user message + its answer); each run
                            // expands to its model turns, each turn to text + tool args
                            // + result.
                            let runs = cluster_runs(&events.get());
                            let total = runs.len();
                            view! {
                                <div class="ev-filter">
                                    <button class="filter-btn" on:click=move |_| show_thinking.update(|t| *t = !*t)>
                                        {move || if show_thinking.get() { "hide thinking" } else { "show thinking" }}
                                    </button>
                                </div>
                                <div class="ev-list">
                                    {if total == 0 {
                                        view! { <div class="empty">"No runs yet."</div> }.into_any()
                                    } else {
                                        runs.into_iter().enumerate().rev()
                                            .map(|(i, r)| render_run(i, total, r, st))
                                            .collect_view().into_any()
                                    }}
                                </div>
                            }
                        })}

                        // STATE
                        {move || (inspect_tab.get() == "state").then(|| {
                            match state_json.get() {
                                None => view! { <div class="empty">"Loading state…"</div> }.into_any(),
                                Some(s) => render_state(&s, prompt_assembled.get(), prompt_assembled, fetch_state).into_any(),
                            }
                        })}
                    </div>
                </section>
            </div>
        </div>
    }
}

// ── chat transcript rendering ────────────────────────────────────────────

fn render_item(item: Item) -> AnyView {
    match item {
        Item::User(text) => view! {
            <div class="msg-user"><div class="bubble-user">{text}</div></div>
        }.into_any(),
        Item::Assistant(text) => view! {
            <div class="msg-assistant">
                <div class="avatar">"⟡"</div>
                <div class="assistant-body"><div class="prose" inner_html=md_to_html(&text)></div></div>
            </div>
        }.into_any(),
        Item::Thinking(text) => view! {
            <div class="msg-assistant">
                <div class="avatar">"⟡"</div>
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
    let dur = if t.duration_ms > 0 { format!("· {}ms", t.duration_ms) } else { String::new() };
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
                <details class="tool-sec" open=true>
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
fn render_run(idx: usize, total: usize, r: RunCluster, show_thinking: bool) -> AnyView {
    let dot_cls = if r.running { "run-dot running" } else if r.errored { "run-dot error" } else { "run-dot" };
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
    let turn_views = r.turns.into_iter().enumerate()
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
        format!("stop_reason: {} · tool_calls: {}",
            t.stop_reason.clone().unwrap_or_else(|| "—".into()), calls)
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
    let dur = if t.duration_ms > 0 { format!("{}ms", t.duration_ms) } else { String::new() };
    let mut body = String::new();
    if !t.input.is_empty() {
        body.push_str(&t.input);
    }
    if !t.result.is_empty() {
        if !body.is_empty() { body.push('\n'); }
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

/// Cluster a flat event list (live wire events OR persisted `{seq,event}`
/// entries) into Run → Turn → details. Coalesces token deltas; never one row
/// per token.
fn cluster_runs(events: &[Value]) -> Vec<RunCluster> {
    let mut runs: Vec<RunCluster> = Vec::new();
    for entry in events {
        let (disc, ev): (String, &Value) = match entry.get("event") {
            Some(inner) => (inner.get("kind").and_then(|v| v.as_str()).unwrap_or("").to_string(), inner),
            None => (entry.get("type").and_then(|v| v.as_str()).unwrap_or("").to_string(), entry),
        };
        match disc.as_str() {
            // UI-injected boundary for live runs (carries the user prompt).
            "run_begin" => {
                let prompt = ev.get("prompt").and_then(|v| v.as_str()).unwrap_or("").to_string();
                runs.push(RunCluster { prompt, running: true, ..Default::default() });
            }
            "run_start" | "RunStart" => {
                let id = ev.get("run_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                runs.push(RunCluster { id, running: true, ..Default::default() });
            }
            "assistant_text_delta" => {
                let t = ev.get("text").and_then(|v| v.as_str()).unwrap_or("");
                cur_turn(&mut runs).text.push_str(t);
            }
            "assistant_thinking_delta" => {
                let t = ev.get("text").and_then(|v| v.as_str()).unwrap_or("");
                cur_turn(&mut runs).thinking.push_str(t);
            }
            "tool_start" => {
                let id = ev.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let name = ev.get("name").and_then(|v| v.as_str()).unwrap_or("tool").to_string();
                cur_turn(&mut runs).tools.push(ToolView { id, name, status: "running".into(), ..Default::default() });
            }
            "tool_dispatching" => {
                let id = ev.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let input = ev.get("input").map(pretty_json).unwrap_or_default();
                if let Some(t) = find_tool(&mut runs, &id) {
                    t.input = input;
                }
            }
            "tool_finish" => {
                let id = ev.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let is_err = ev.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false);
                let preview = ev.get("preview").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let dur = ev.get("duration_ms").and_then(|v| v.as_u64()).unwrap_or(0);
                let sources = parse_sources(ev.get("metadata"));
                if let Some(t) = find_tool(&mut runs, &id) {
                    t.status = if is_err { "error".into() } else { "done".into() };
                    t.result = preview;
                    t.duration_ms = dur;
                    t.sources = sources;
                }
            }
            "Message" => ingest_persisted(&mut runs, ev.get("msg")),
            "turn_complete" | "TurnBoundary" => {
                if let Some(run) = runs.last_mut()
                    && let Some(turn) = run.turns.last_mut() {
                        turn.closed = true;
                        if let Some(sr) = ev.get("stop_reason").and_then(|v| v.as_str()) {
                            turn.stop_reason = Some(sr.to_string());
                        }
                        match ev.get("tool_calls_this_turn").and_then(|v| v.as_u64()) {
                            Some(tc) => turn.tool_calls = tc as u32,
                            None if turn.tool_calls == 0 => turn.tool_calls = turn.tools.len() as u32,
                            None => {}
                        }
                    }
            }
            "run_end" | "RunEnd" | "done" => {
                if let Some(run) = runs.last_mut() {
                    run.running = false;
                    run.ended = true;
                    if let Some(sr) = ev.get("stop_reason").and_then(|v| v.as_str()) {
                        run.stop_reason = Some(sr.to_string());
                    }
                    if let Some(t) = run.turns.last_mut() {
                        t.closed = true;
                        if t.tool_calls == 0 { t.tool_calls = t.tools.len() as u32; }
                    }
                }
            }
            "usage" => {
                if let Some(run) = runs.last_mut() {
                    let i = ev.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                    let o = ev.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                    run.usage = Some((i, o));
                }
            }
            "warning" => {
                if let Some(run) = runs.last_mut() {
                    run.errored = true;
                }
            }
            _ => {}
        }
    }
    runs
}

/// Current open turn of the current run, creating a run/turn as needed.
fn cur_turn(runs: &mut Vec<RunCluster>) -> &mut TurnCluster {
    let need_new_run = match runs.last() {
        Some(r) => r.ended,
        None => true,
    };
    if need_new_run {
        runs.push(RunCluster { running: true, ..Default::default() });
    }
    let run = runs.last_mut().unwrap();
    let need_new = match run.turns.last() {
        Some(t) => t.closed,
        None => true,
    };
    if need_new {
        run.turns.push(TurnCluster::default());
    }
    run.turns.last_mut().unwrap()
}

/// Find a tool (by id) in the current run, searching newest-first.
fn find_tool<'a>(runs: &'a mut [RunCluster], id: &str) -> Option<&'a mut ToolView> {
    let run = runs.last_mut()?;
    for turn in run.turns.iter_mut().rev() {
        if let Some(t) = turn.tools.iter_mut().rev().find(|t| t.id == id) {
            return Some(t);
        }
    }
    None
}

/// Fold a persisted `Message` into the run/turn clusters.
fn ingest_persisted(runs: &mut Vec<RunCluster>, msg: Option<&Value>) {
    let Some(msg) = msg else { return };
    let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
    let blocks = msg.get("content").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    if role == "assistant" {
        let turn = cur_turn(runs);
        for b in &blocks {
            match b.get("type").and_then(|v| v.as_str()) {
                Some("text") => {
                    if let Some(t) = b.get("text").and_then(|v| v.as_str()) { turn.text.push_str(t); }
                }
                Some("reasoning") => {
                    if let Some(t) = b.get("text").and_then(|v| v.as_str()) { turn.thinking.push_str(t); }
                }
                Some("tool_use") => {
                    turn.tools.push(ToolView {
                        id: b.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                        name: b.get("name").and_then(|v| v.as_str()).unwrap_or("tool").to_string(),
                        input: b.get("input").map(pretty_json).unwrap_or_default(),
                        status: "done".into(),
                        ..Default::default()
                    });
                }
                _ => {}
            }
        }
        turn.tool_calls = turn.tools.len() as u32;
    } else if role == "user" {
        // Plain user text is the run's prompt; tool_result blocks attach to
        // the tool they answer.
        if let Some(run) = runs.last_mut()
            && run.prompt.is_empty() {
                for b in &blocks {
                    if b.get("type").and_then(|v| v.as_str()) == Some("text")
                        && let Some(t) = b.get("text").and_then(|v| v.as_str()) {
                            run.prompt = t.to_string();
                        }
                }
            }
        for b in &blocks {
            if b.get("type").and_then(|v| v.as_str()) == Some("tool_result") {
                let id = b.get("tool_use_id").and_then(|v| v.as_str()).unwrap_or("");
                let is_err = b.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false);
                let content = b.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let sources = parse_sources(b.get("metadata"));
                if let Some(t) = find_tool(runs, id) {
                    t.status = if is_err { "error".into() } else { "done".into() };
                    t.result = truncate(&content, 400);
                    t.sources = sources;
                }
            }
        }
    }
}

fn clean_tool_name(n: &str) -> String {
    if let Some(rest) = n.strip_prefix("mcp__") {
        let parts: Vec<&str> = rest.splitn(2, "__").collect();
        if parts.len() == 2 {
            return format!("{} · {}", parts[0], parts[1]);
        }
    }
    n.to_string()
}

fn pretty_json(v: &Value) -> String {
    truncate(&serde_json::to_string_pretty(v).unwrap_or_default(), 600)
}

// ── live → items folding (chat pane) ─────────────────────────────────────

fn append_live(live: RwSignal<LiveBuf>, items: RwSignal<Vec<Item>>, kind: LiveKind, text: &str) {
    live.update(|lb| {
        if lb.kind != kind && !lb.text.is_empty() {
            items.update(|its| its.push(finalize(lb.kind, &lb.text)));
            lb.text.clear();
        }
        lb.kind = kind;
        lb.text.push_str(text);
    });
}

fn flush_live(live: RwSignal<LiveBuf>, items: RwSignal<Vec<Item>>) {
    live.update(|lb| {
        if !lb.text.is_empty() {
            items.update(|its| its.push(finalize(lb.kind, &lb.text)));
        }
        lb.kind = LiveKind::None;
        lb.text.clear();
    });
}

fn finalize(kind: LiveKind, text: &str) -> Item {
    match kind {
        LiveKind::Thinking => Item::Thinking(text.to_string()),
        _ => Item::Assistant(text.to_string()),
    }
}

fn items_from_events(events: &[Value]) -> Vec<Item> {
    let mut items = Vec::new();
    for entry in events {
        let event = entry.get("event").unwrap_or(entry);
        match event.get("kind").and_then(|v| v.as_str()) {
            Some("Message") => {
                if let Some(msg) = event.get("msg") {
                    ingest_message(&mut items, msg);
                }
            }
            Some("StateSnapshot") => {
                if let Some(msgs) = event.get("messages").and_then(|v| v.as_array()) {
                    items.clear();
                    for m in msgs {
                        ingest_message(&mut items, m);
                    }
                }
            }
            _ => {}
        }
    }
    items
}

fn ingest_message(items: &mut Vec<Item>, msg: &Value) {
    let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
    let blocks = msg.get("content").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    for b in &blocks {
        match b.get("type").and_then(|v| v.as_str()) {
            Some("text") => {
                let t = b.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string();
                if t.is_empty() {
                    continue;
                }
                if role == "user" {
                    items.push(Item::User(t));
                } else {
                    items.push(Item::Assistant(t));
                }
            }
            Some("tool_use") => {
                items.push(Item::Tool(ToolView {
                    id: b.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    name: b.get("name").and_then(|v| v.as_str()).unwrap_or("tool").to_string(),
                    input: b.get("input").map(|v| truncate(&v.to_string(), 300)).unwrap_or_default(),
                    status: "done".to_string(),
                    ..Default::default()
                }));
            }
            Some("tool_result") => {
                let id = b.get("tool_use_id").and_then(|v| v.as_str()).unwrap_or("");
                let is_error = b.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false);
                let content = b.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let sources = parse_sources(b.get("metadata"));
                if let Some(Item::Tool(t)) = items
                    .iter_mut()
                    .rev()
                    .find(|i| matches!(i, Item::Tool(t) if t.id == id))
                {
                    t.status = if is_error { "error".into() } else { "done".into() };
                    t.result = truncate(&content, 600);
                    t.sources = sources;
                }
            }
            _ => {}
        }
    }
}

fn apply_event(items: &mut Vec<Item>, ev: &Value) {
    let kind = ev.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match kind {
        "assistant_text_delta" => {
            let text = ev.get("text").and_then(|v| v.as_str()).unwrap_or("");
            match items.last_mut() {
                Some(Item::Assistant(s)) => s.push_str(text),
                _ => items.push(Item::Assistant(text.to_string())),
            }
        }
        "assistant_thinking_delta" => {
            let text = ev.get("text").and_then(|v| v.as_str()).unwrap_or("");
            match items.last_mut() {
                Some(Item::Thinking(s)) => s.push_str(text),
                _ => items.push(Item::Thinking(text.to_string())),
            }
        }
        "tool_start" => {
            items.push(Item::Tool(ToolView {
                id: ev.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                name: ev.get("name").and_then(|v| v.as_str()).unwrap_or("tool").to_string(),
                status: "running".to_string(),
                ..Default::default()
            }));
        }
        "tool_dispatching" => {
            let id = ev.get("id").and_then(|v| v.as_str()).unwrap_or("");
            if let Some(Item::Tool(t)) = items.iter_mut().rev().find(|i| matches!(i, Item::Tool(t) if t.id == id)) {
                t.input = ev.get("input").map(|v| truncate(&v.to_string(), 300)).unwrap_or_default();
            }
        }
        "tool_finish" => {
            let id = ev.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let is_error = ev.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false);
            let preview = ev.get("preview").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let dur = ev.get("duration_ms").and_then(|v| v.as_u64()).unwrap_or(0);
            let sources = parse_sources(ev.get("metadata"));
            if let Some(Item::Tool(t)) = items.iter_mut().rev().find(|i| matches!(i, Item::Tool(t) if t.id == id)) {
                t.status = if is_error { "error".into() } else { "done".into() };
                t.result = preview;
                t.duration_ms = dur;
                t.sources = sources;
            }
        }
        "warning" => {
            let m = ev.get("message").and_then(|v| v.as_str()).unwrap_or("").to_string();
            items.push(Item::Warning(m));
        }
        "message" => {
            if let Some(msg) = ev.get("msg") {
                let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
                if role == "user" {
                    if let Some(text) = first_text(msg)
                        && !matches!(items.last(), Some(Item::User(u)) if *u == text) {
                            items.push(Item::User(text));
                        }
                } else if role == "assistant"
                    && let Some(text) = first_text(msg)
                        && !matches!(items.last(), Some(Item::Assistant(_))) {
                            items.push(Item::Assistant(text));
                        }
            }
        }
        _ => {}
    }
}

fn first_text(msg: &Value) -> Option<String> {
    msg.get("content")?.as_array()?.iter().find_map(|b| {
        if b.get("type").and_then(|v| v.as_str()) == Some("text") {
            b.get("text").and_then(|v| v.as_str()).map(|s| s.to_string())
        } else {
            None
        }
    })
}

fn parse_sources(metadata: Option<&Value>) -> Vec<Source> {
    let Some(arr) = metadata.and_then(|m| m.get("sources")).and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|s| {
            let url = s.get("url").and_then(|v| v.as_str())?.to_string();
            let title = s.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string();
            Some(Source { title, url })
        })
        .collect()
}

fn parse_pending(ev: &Value) -> Option<PendingApproval> {
    let call_id = ev.get("call_id")?.as_str()?.to_string();
    let tool_name = ev.get("tool_name").and_then(|v| v.as_str()).unwrap_or("tool").to_string();
    let draft = ev.get("draft")?;
    let summary = draft.get("summary").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let current_input = draft.get("current_input").cloned().unwrap_or_else(|| serde_json::json!({}));
    let fields = draft
        .get("editable_fields")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|f| f.as_str())
                .map(|name| {
                    let cur = current_input.get(name).and_then(|v| v.as_str()).unwrap_or("").to_string();
                    (name.to_string(), cur)
                })
                .collect()
        })
        .unwrap_or_default();
    Some(PendingApproval { call_id, tool_name, summary, current_input, fields })
}

fn usage_of(ev: &Value) -> Option<(&'static str, (u64, u64))> {
    if ev.get("type").and_then(|v| v.as_str()) == Some("usage") {
        let i = ev.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
        let o = ev.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
        Some(("usage", (i, o)))
    } else {
        None
    }
}

// ── state tab ────────────────────────────────────────────────────────────

fn render_state(
    s: &Value,
    assembled: bool,
    prompt_assembled: RwSignal<bool>,
    refresh: impl Fn() + Copy + 'static,
) -> AnyView {
    let busy = s.get("busy").and_then(|v| v.as_bool()).unwrap_or(false);
    let assembled_prompt = s.get("assembled_system_prompt").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let base_prompt = s.get("base_system_prompt").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let event_count = s.get("event_count").and_then(|v| v.as_u64()).unwrap_or(0);
    let run_count = s.get("run_count").and_then(|v| v.as_u64()).map(|r| r.to_string()).unwrap_or_else(|| "—".into());
    let messages = s.get("messages").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let tools = s.get("tools").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let msg_count = messages.len();
    let tool_count = tools.len();
    let prompt = if assembled { assembled_prompt } else { base_prompt };

    let tool_views = tools.iter().map(|t| {
        let name = t.get("name").and_then(|v| v.as_str()).unwrap_or("?").to_string();
        let desc = t.get("description").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let schema = t.get("input_schema").map(|v| serde_json::to_string_pretty(v).unwrap_or_default()).unwrap_or_default();
        view! {
            <details class="state-tool">
                <summary><span class="nm">{name}</span><span class="desc">{desc}</span></summary>
                <pre class="jsonpre">{schema}</pre>
            </details>
        }
    }).collect_view();

    let msg_views = messages.into_iter().map(|m| {
        let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("?").to_string();
        let body = m.get("content").and_then(|v| v.as_array()).map(|blocks| {
            blocks.iter().map(render_block_summary).collect::<Vec<_>>().join("\n")
        }).unwrap_or_default();
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
                        <div class="seg">
                            <button class:on=move || prompt_assembled.get()
                                on:click=move |_| prompt_assembled.set(true)>"assembled"</button>
                            <button class:on=move || !prompt_assembled.get()
                                on:click=move |_| prompt_assembled.set(false)>"base"</button>
                        </div>
                    </div>
                    <div class="prompt-view">{prompt}</div>
                </div>

                <div>
                    <div class="state-section-head">
                        <span class="state-section-cap">"Tools"</span>
                        <span class="state-count">{tool_count.to_string()}</span>
                    </div>
                    <div class="state-tools">{tool_views}</div>
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

fn render_block_summary(b: &Value) -> String {
    match b.get("type").and_then(|v| v.as_str()) {
        Some("text") => b.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        Some("tool_use") => format!(
            "→ tool_use {}({})",
            b.get("name").and_then(|v| v.as_str()).unwrap_or("?"),
            b.get("input").map(|v| truncate(&v.to_string(), 120)).unwrap_or_default()
        ),
        Some("tool_result") => format!(
            "← tool_result {}",
            truncate(b.get("content").and_then(|v| v.as_str()).unwrap_or(""), 120)
        ),
        Some("image") => "[image]".to_string(),
        Some("reasoning") => format!("[thinking] {}", truncate(b.get("text").and_then(|v| v.as_str()).unwrap_or(""), 120)),
        Some("blob") => "[blob]".to_string(),
        other => format!("[{}]", other.unwrap_or("?")),
    }
}

// ── misc helpers ─────────────────────────────────────────────────────────

fn md_to_html(src: &str) -> String {
    use pulldown_cmark::{html, Options, Parser};
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TABLES);
    let parser = Parser::new_ext(src, opts);
    let mut out = String::new();
    html::push_html(&mut out, parser);
    out
}

fn short_id(id: &str) -> String {
    if id.len() > 12 { format!("{}…", &id[..12]) } else { id.to_string() }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(kind: &str, msg: Value) -> Value {
        serde_json::json!({ "seq": 1, "event": { "kind": kind, "msg": msg } })
    }

    #[test]
    fn reconstructs_history_with_lowercase_roles() {
        let events = vec![
            ev("RunStart", Value::Null),
            ev("Message", serde_json::json!({ "role": "user", "content": [{ "type": "text", "text": "hi" }] })),
            ev("Message", serde_json::json!({ "role": "assistant", "content": [{ "type": "text", "text": "hello!" }] })),
        ];
        let items = items_from_events(&events);
        assert_eq!(items.len(), 2);
        assert!(matches!(&items[0], Item::User(t) if t == "hi"));
        assert!(matches!(&items[1], Item::Assistant(t) if t == "hello!"));
    }

    #[test]
    fn pairs_tool_use_with_its_result() {
        let events = vec![
            ev("Message", serde_json::json!({ "role": "assistant", "content": [
                { "type": "tool_use", "id": "call_1", "name": "echo", "input": { "msg": "x" } }
            ] })),
            ev("Message", serde_json::json!({ "role": "user", "content": [
                { "type": "tool_result", "tool_use_id": "call_1", "content": "ran echo", "is_error": false }
            ] })),
        ];
        let items = items_from_events(&events);
        assert_eq!(items.len(), 1);
        match &items[0] {
            Item::Tool(t) => {
                assert_eq!(t.name, "echo");
                assert_eq!(t.status, "done");
                assert!(t.result.contains("ran echo"));
            }
            other => panic!("expected Tool, got {other:?}"),
        }
    }

    #[test]
    fn clusters_live_run_into_turns() {
        let events = vec![
            serde_json::json!({ "type": "run_start", "run_id": "r1" }),
            serde_json::json!({ "type": "assistant_text_delta", "text": "Look" }),
            serde_json::json!({ "type": "assistant_text_delta", "text": "ing." }),
            serde_json::json!({ "type": "tool_start", "id": "c1", "name": "mcp__tavily__search" }),
            serde_json::json!({ "type": "tool_dispatching", "id": "c1", "input": { "q": "x" } }),
            serde_json::json!({ "type": "tool_finish", "id": "c1", "is_error": false, "preview": "ok", "duration_ms": 12 }),
            serde_json::json!({ "type": "turn_complete", "stop_reason": "tool_use", "tool_calls_this_turn": 1 }),
            serde_json::json!({ "type": "assistant_text_delta", "text": "Done" }),
            serde_json::json!({ "type": "run_end", "run_id": "r1", "total_turns": 2, "stop_reason": "end_turn" }),
            serde_json::json!({ "type": "usage", "input_tokens": 100, "output_tokens": 20 }),
        ];
        let runs = cluster_runs(&events);
        assert_eq!(runs.len(), 1);
        let r = &runs[0];
        assert_eq!(r.id, "r1");
        assert!(!r.running);
        assert_eq!(r.usage, Some((100, 20)));
        // text coalesced, not one turn per token
        assert_eq!(r.turns.len(), 2);
        assert_eq!(r.turns[0].text, "Looking.");
        assert_eq!(r.turns[0].tools.len(), 1);
        assert_eq!(r.turns[0].tools[0].status, "done");
        assert_eq!(r.turns[0].stop_reason.as_deref(), Some("tool_use"));
        assert_eq!(r.turns[1].text, "Done");
    }

    #[test]
    fn run_begin_separates_live_runs_and_keeps_prompt() {
        // Two send cycles: each run_begin..done is its own run; the synthetic
        // marker carries the user prompt (the live stream omits it).
        let events = vec![
            serde_json::json!({ "type": "run_begin", "prompt": "first question" }),
            serde_json::json!({ "type": "assistant_text_delta", "text": "answer one" }),
            serde_json::json!({ "type": "done", "total_turns": 1 }),
            serde_json::json!({ "type": "run_begin", "prompt": "second question" }),
            serde_json::json!({ "type": "assistant_text_delta", "text": "answer two" }),
            serde_json::json!({ "type": "done", "total_turns": 1 }),
        ];
        let runs = cluster_runs(&events);
        assert_eq!(runs.len(), 2, "each run_begin..done is a distinct run");
        assert_eq!(runs[0].prompt, "first question");
        assert_eq!(runs[0].turns[0].text, "answer one");
        assert!(runs[0].ended);
        assert_eq!(runs[1].prompt, "second question");
        assert_eq!(runs[1].turns[0].text, "answer two");
    }

    #[test]
    fn state_snapshot_replaces_history() {
        let events = vec![
            ev("Message", serde_json::json!({ "role": "user", "content": [{ "type": "text", "text": "old" }] })),
            serde_json::json!({ "seq": 2, "event": { "kind": "StateSnapshot", "messages": [
                { "role": "user", "content": [{ "type": "text", "text": "summary" }] }
            ] } }),
        ];
        let items = items_from_events(&events);
        assert_eq!(items.len(), 1);
        assert!(matches!(&items[0], Item::User(t) if t == "summary"));
    }
}
