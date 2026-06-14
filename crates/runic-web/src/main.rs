//! runic dev console — a Leptos CSR app for driving `runic serve`.
//!
//! Three panes: thread list (left), chat transcript with streaming text +
//! tool-call cards (center), live state/event inspector (right). Talks to
//! the server over HTTP + SSE; events are parsed leniently as JSON so the
//! UI never hard-couples to the server's internal `WireEvent` type.

use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::Value;

mod api;
use api::ApiClient;

/// The actively-streaming tail. Tokens append here (one reactive text node)
/// instead of mutating the `items` list, so per-token cost is O(1) DOM
/// instead of re-rendering the whole transcript. On a boundary (a non-text
/// event) or run end it flushes into `items` as a finalized, markdown-
/// rendered message.
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
    /// Assistant text, accumulated across `assistant_text_delta` events.
    Assistant(String),
    /// Hidden reasoning, accumulated across `assistant_thinking_delta`.
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
    /// Client-facing grounding sources pulled from tool-result `metadata`.
    sources: Vec<Source>,
}

#[derive(Clone, Debug)]
struct Source {
    title: String,
    url: String,
}

/// A HITL tool waiting for the operator's decision. Only one is live at a
/// time (HITL dispatch serializes), so the app holds a single `Option`.
/// Field edits are written back into the `pending` signal by index, so we
/// don't create per-field signals from the (owner-less) stream callback.
#[derive(Clone)]
struct PendingApproval {
    call_id: String,
    tool_name: String,
    summary: String,
    /// Full current input, used as the base when submitting; edited fields
    /// are overlaid on top.
    current_input: serde_json::Value,
    /// Editable `(name, value)` pairs, prefilled from `current_input`.
    fields: Vec<(String, String)>,
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
    let usage = RwSignal::new(None::<(u64, u64)>); // (input, output)
    let input = RwSignal::new(String::new());
    let streaming = RwSignal::new(false);
    let pending = RwSignal::new(None::<PendingApproval>);
    // Gate the card on presence only, so per-keystroke field edits (which
    // update `pending`) don't rebuild the card and steal input focus.
    let has_pending = Memo::new(move |_| pending.get().is_some());

    // Cancel: the active run's AbortController, so a stop button can abort
    // the fetch stream.
    let abort = RwSignal::new(None::<web_sys::AbortController>);
    // Right pane: "events" (live log) vs "state" (prompt + messages).
    let inspect_tab = RwSignal::new("events");
    let state_json = RwSignal::new(None::<Value>);

    // Auto-scroll the transcript to the bottom as content arrives.
    let transcript_ref = NodeRef::<leptos::html::Div>::new();
    Effect::new(move |_| {
        items.track();
        live.track();
        if let Some(el) = transcript_ref.get() {
            el.set_scroll_top(el.scroll_height());
        }
    });

    let client = move || ApiClient::new(api_base.get_untracked(), tenant.get_untracked());

    // Refresh the thread list from the server.
    let refresh_threads = move || {
        let c = client();
        spawn_local(async move {
            match c.list_threads().await {
                Ok(ids) => threads.set(ids),
                Err(e) => leptos::logging::warn!("list_threads failed: {e}"),
            }
        });
    };

    // Load a thread's history into the transcript + inspector.
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
                    let mut its = Vec::new();
                    for ev in &evs {
                        apply_event(&mut its, ev);
                    }
                    items.set(its);
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

    // Send the composer text as a run and stream the reply.
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
        input.set(String::new());
        items.update(|its| its.push(Item::User(text.clone())));
        streaming.set(true);

        // Fresh AbortController so the stop button can cancel this run.
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
                    _ => {
                        flush_live(live, items);
                        items.update(|its| apply_event(its, &ev));
                    }
                }
            };
            let result = c.stream_run(&thread_id, &text, signal.as_ref(), on_event).await;
            flush_live(live, items);
            if let Err(e) = result {
                // An aborted fetch isn't an error worth surfacing.
                if !e.to_lowercase().contains("abort") {
                    items.update(|its| its.push(Item::Warning(format!("stream error: {e}"))));
                } else {
                    items.update(|its| its.push(Item::Warning("— stopped —".into())));
                }
            }
            streaming.set(false);
            abort.set(None);
            // The run is over; any unanswered approval is moot.
            pending.set(None);
        });
    };

    // Stop the active run: aborts the fetch stream client-side. (The server
    // run finishes its current turn; this just stops the UI waiting.)
    let stop = move || {
        if let Some(c) = abort.get_untracked() {
            c.abort();
        }
    };

    // Fetch the full thread state (system prompt + messages) for the state tab.
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

    // Initial population.
    refresh_threads();

    view! {
        <div class="app">
            <aside class="threads">
                <div class="brand">"⟡ runic"<span class="brand-sub">"dev console"</span></div>
                <div class="conn">
                    <label class="conn-label">"server"</label>
                    <input class="mini" prop:value=move || api_base.get()
                        on:input=move |e| api_base.set(event_target_value(&e)) placeholder="http://127.0.0.1:8920" />
                    <label class="conn-label">"tenant"</label>
                    <input class="mini" prop:value=move || tenant.get()
                        on:input=move |e| tenant.set(event_target_value(&e)) placeholder="default" />
                </div>
                <div class="threads-head">
                    <span>"threads"</span>
                    <button class="icon-btn" title="refresh" on:click=move |_| refresh_threads()>"⟳"</button>
                </div>
                <button class="newthread" on:click=move |_| new_thread()>"+ new thread"</button>
                <ul>
                    {move || threads.get().into_iter().map(|id| {
                        let id2 = id.clone();
                        let is_cur = move || current.get().as_deref() == Some(id2.as_str());
                        let id3 = id.clone();
                        let short = short_id(&id);
                        view! {
                            <li class:active=is_cur on:click=move |_| load_thread(id3.clone())>
                                {short}
                            </li>
                        }
                    }).collect_view()}
                </ul>
            </aside>

            <main class="chat">
                <div class="pane-head">
                    {move || current.get().map(|id| short_id(&id)).unwrap_or_else(|| "no thread selected".into())}
                    {move || streaming.get().then(|| view! { <span class="spin">" ● streaming"</span> })}
                </div>
                <div class="transcript" node_ref=transcript_ref>
                    {move || (items.get().is_empty() && live.get().text.is_empty()).then(|| {
                        let msg = if current.get().is_some() {
                            "Send a message to start the conversation."
                        } else {
                            "Create or select a thread to begin."
                        };
                        view! { <div class="empty">{msg}</div> }
                    })}
                    {move || items.get().into_iter().map(render_item).collect_view()}
                    // Live streaming tail — plain text, updated per token.
                    {move || {
                        let lb = live.get();
                        if lb.text.is_empty() {
                            ().into_any()
                        } else if matches!(lb.kind, LiveKind::Thinking) {
                            view! { <div class="msg thinking-live">{lb.text}</div> }.into_any()
                        } else {
                            view! {
                                <div class="msg assistant">
                                    <div class="avatar bot-av">"ai"</div>
                                    <div class="body live">{lb.text}</div>
                                </div>
                            }.into_any()
                        }
                    }}
                </div>
                {move || has_pending.get().then(|| {
                    let p = pending.get_untracked().expect("has_pending implies Some");
                    let field_views = p.fields.iter().enumerate().map(|(i, (name, val))| {
                        view! {
                            <label class="apv-field">
                                <span>{name.clone()}</span>
                                <input value=val.clone()
                                    on:input=move |e| pending.update(|opt| {
                                        if let Some(p) = opt {
                                            if let Some(f) = p.fields.get_mut(i) { f.1 = event_target_value(&e); }
                                        }
                                    }) />
                            </label>
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
                        <div class="approval">
                            <div class="apv-head">"⚠ approval required · " {p.tool_name.clone()}</div>
                            <div class="apv-summary">{p.summary.clone()}</div>
                            {field_views}
                            <div class="apv-actions">
                                <button class="apv-approve" on:click=approve>"approve"</button>
                                <button class="apv-cancel" on:click=cancel>"cancel"</button>
                            </div>
                        </div>
                    }
                })}
                <div class="composer">
                    <input
                        prop:value=move || input.get()
                        on:input=move |e| input.set(event_target_value(&e))
                        on:keydown=move |e| if e.key() == "Enter" { send() }
                        placeholder=move || if current.get().is_some() { "type a message…".to_string() } else { "create or pick a thread first".to_string() }
                        disabled=move || current.get().is_none() || streaming.get()
                    />
                    {move || if streaming.get() {
                        view! { <button class="stop" on:click=move |_| stop()>"stop"</button> }.into_any()
                    } else {
                        view! { <button on:click=move |_| send()>"send"</button> }.into_any()
                    }}
                </div>
            </main>

            <aside class="inspect">
                <div class="tabs">
                    <button class="tab" class:on=move || inspect_tab.get() == "events"
                        on:click=move |_| inspect_tab.set("events")>"events"</button>
                    <button class="tab" class:on=move || inspect_tab.get() == "state"
                        on:click=move |_| { inspect_tab.set("state"); fetch_state(); }>"state"</button>
                    <span class="tab-usage">
                        {move || match usage.get() {
                            Some((i, o)) => format!("in {i} · out {o}"),
                            None => "—".into(),
                        }}
                    </span>
                </div>

                {move || if inspect_tab.get() == "events" {
                    view! {
                        <div class="events">
                            {move || events.get().into_iter().rev().map(|ev| {
                                let kind = ev.get("type").and_then(|v| v.as_str()).unwrap_or("?").to_string();
                                let body = serde_json::to_string(&ev).unwrap_or_default();
                                view! { <div class="evt"><span class="evt-kind">{kind}</span><code>{truncate(&body, 240)}</code></div> }
                            }).collect_view()}
                        </div>
                    }.into_any()
                } else {
                    view! {
                        <div class="state-view">
                            <button class="refresh-state" on:click=move |_| fetch_state()>"⟳ refresh"</button>
                            {move || match state_json.get() {
                                None => view! { <div class="empty small">"Loading state…"</div> }.into_any(),
                                Some(s) => render_state(&s).into_any(),
                            }}
                        </div>
                    }.into_any()
                }}
            </aside>
        </div>
    }
}

fn render_item(item: Item) -> AnyView {
    match item {
        Item::User(text) => view! {
            <div class="msg user">
                <div class="avatar user-av">"you"</div>
                <div class="bubble">{text}</div>
            </div>
        }.into_any(),
        Item::Assistant(text) => view! {
            <div class="msg assistant">
                <div class="avatar bot-av">"ai"</div>
                <div class="body md" inner_html=md_to_html(&text)></div>
            </div>
        }.into_any(),
        Item::Thinking(text) => view! {
            <details class="msg thinking">
                <summary>"thinking"</summary>
                <div class="body md" inner_html=md_to_html(&text)></div>
            </details>
        }.into_any(),
        Item::Warning(text) => view! { <div class="msg warn">{text}</div> }.into_any(),
        Item::Tool(t) => {
            let badge = if t.status == "done" { "✓" } else if t.status == "error" { "✗" } else { "⟳" };
            let dur = if t.duration_ms > 0 { format!(" {}ms", t.duration_ms) } else { String::new() };
            let sources = t.sources.clone();
            view! {
                <div class="tool" class:err=move || t.status == "error">
                    <div class="tool-head">
                        <span class="tool-badge">{badge}</span>
                        <span class="tool-name">{t.name.clone()}</span>
                        <span class="tool-dur">{dur}</span>
                    </div>
                    {(!t.input.is_empty()).then(|| view! { <code class="tool-io">{format!("→ {}", t.input)}</code> })}
                    {(!t.result.is_empty()).then(|| view! { <code class="tool-io result">{t.result.clone()}</code> })}
                    {(!sources.is_empty()).then(move || view! {
                        <div class="sources">
                            {sources.iter().map(|s| {
                                let url = s.url.clone();
                                view! { <a class="chip" href=url.clone() target="_blank">{format!("🔗 {}", if s.title.is_empty() { s.url.clone() } else { s.title.clone() })}</a> }
                            }).collect_view()}
                        </div>
                    })}
                </div>
            }.into_any()
        }
    }
}

/// Append a streaming token to the live buffer. On a kind change (text →
/// thinking or vice-versa) the existing buffer is finalized into `items`
/// first.
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

/// Flush any buffered live text into `items` as a finalized message.
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

/// Fold one parsed event into the transcript items.
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
        // history replay carries full `message` events; surface user turns
        // that we didn't originate locally (assistant text already streamed).
        "message" => {
            if let Some(msg) = ev.get("msg") {
                let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
                if role == "User" {
                    if let Some(text) = first_text(msg) {
                        if !matches!(items.last(), Some(Item::User(u)) if *u == text) {
                            items.push(Item::User(text));
                        }
                    }
                } else if role == "Assistant" {
                    if let Some(text) = first_text(msg) {
                        if !matches!(items.last(), Some(Item::Assistant(_))) {
                            items.push(Item::Assistant(text));
                        }
                    }
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

/// Render the full thread-state snapshot: system prompt + counts + the
/// message list as the model sees it.
fn render_state(s: &Value) -> AnyView {
    let busy = s.get("busy").and_then(|v| v.as_bool()).unwrap_or(false);
    let system_prompt = s.get("system_prompt").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let events = s.get("event_count").and_then(|v| v.as_u64()).unwrap_or(0);
    let runs = s.get("run_count").and_then(|v| v.as_u64()).unwrap_or(0);
    let messages = s.get("messages").and_then(|v| v.as_array()).cloned().unwrap_or_default();

    let msg_views = messages.into_iter().map(|m| {
        let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("?").to_string();
        let body = m.get("content").and_then(|v| v.as_array()).map(|blocks| {
            blocks.iter().map(render_block_summary).collect::<Vec<_>>().join("\n")
        }).unwrap_or_default();
        let cls = format!("state-role role-{}", role.to_lowercase());
        view! {
            <div class="state-msg">
                <span class=cls>{role}</span>
                <code>{body}</code>
            </div>
        }
    }).collect_view();

    view! {
        <div>
            {busy.then(|| view! { <div class="state-busy">"⏳ agent busy (run in progress) — messages from store"</div> })}
            <div class="state-counts">{format!("{runs} runs · {events} events · {} messages", msg_views_len(s))}</div>
            <div class="state-section">"system prompt"</div>
            <pre class="state-prompt">{system_prompt}</pre>
            <div class="state-section">"messages (as sent to the model)"</div>
            <div class="state-msgs">{msg_views}</div>
        </div>
    }.into_any()
}

fn msg_views_len(s: &Value) -> usize {
    s.get("messages").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0)
}

/// One-line summary of a content block for the state view.
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

/// Render assistant/thinking markdown to HTML. pulldown-cmark escapes raw
/// text, so model output can't inject arbitrary tags — adequate for a local
/// dev tool. Tables + strikethrough enabled on top of CommonMark.
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
