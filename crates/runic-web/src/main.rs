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
    let events = RwSignal::new(Vec::<Value>::new());
    let usage = RwSignal::new(None::<(u64, u64)>); // (input, output)
    let input = RwSignal::new(String::new());
    let streaming = RwSignal::new(false);

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
        events.set(Vec::new());
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
                    events.set(Vec::new());
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

        spawn_local(async move {
            let on_event = move |ev: Value| {
                events.update(|e| e.push(ev.clone()));
                if let Some(("usage", (i, o))) = usage_of(&ev) {
                    usage.set(Some((i, o)));
                    let _ = "usage";
                }
                items.update(|its| apply_event(its, &ev));
            };
            if let Err(e) = c.stream_run(&thread_id, &text, on_event).await {
                items.update(|its| its.push(Item::Warning(format!("stream error: {e}"))));
            }
            streaming.set(false);
        });
    };

    // Initial population.
    refresh_threads();

    view! {
        <div class="app">
            <aside class="threads">
                <div class="pane-head">"threads"</div>
                <div class="conn">
                    <input class="mini" prop:value=move || api_base.get()
                        on:input=move |e| api_base.set(event_target_value(&e)) placeholder="server url" />
                    <input class="mini" prop:value=move || tenant.get()
                        on:input=move |e| tenant.set(event_target_value(&e)) placeholder="tenant" />
                    <button on:click=move |_| refresh_threads()>"⟳"</button>
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
                <div class="transcript">
                    {move || items.get().into_iter().map(render_item).collect_view()}
                </div>
                <div class="composer">
                    <input
                        prop:value=move || input.get()
                        on:input=move |e| input.set(event_target_value(&e))
                        on:keydown=move |e| if e.key() == "Enter" { send() }
                        placeholder=move || if current.get().is_some() { "type a message…".to_string() } else { "create or pick a thread first".to_string() }
                        disabled=move || current.get().is_none() || streaming.get()
                    />
                    <button on:click=move |_| send() disabled=move || streaming.get()>"send"</button>
                </div>
            </main>

            <aside class="inspect">
                <div class="pane-head">"state"</div>
                <div class="usage">
                    {move || match usage.get() {
                        Some((i, o)) => format!("tokens  in {i}  out {o}"),
                        None => "tokens  —".into(),
                    }}
                </div>
                <div class="events">
                    {move || events.get().into_iter().rev().map(|ev| {
                        let kind = ev.get("type").and_then(|v| v.as_str()).unwrap_or("?").to_string();
                        let body = serde_json::to_string(&ev).unwrap_or_default();
                        view! { <div class="evt"><span class="evt-kind">{kind}</span><code>{truncate(&body, 240)}</code></div> }
                    }).collect_view()}
                </div>
            </aside>
        </div>
    }
}

fn render_item(item: Item) -> AnyView {
    match item {
        Item::User(text) => view! { <div class="msg user"><span class="role">"user"</span><div class="body">{text}</div></div> }.into_any(),
        Item::Assistant(text) => view! { <div class="msg assistant"><span class="role">"assistant"</span><div class="body">{text}</div></div> }.into_any(),
        Item::Thinking(text) => view! { <details class="msg thinking"><summary>"thinking"</summary><div class="body">{text}</div></details> }.into_any(),
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

fn usage_of(ev: &Value) -> Option<(&'static str, (u64, u64))> {
    if ev.get("type").and_then(|v| v.as_str()) == Some("usage") {
        let i = ev.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
        let o = ev.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
        Some(("usage", (i, o)))
    } else {
        None
    }
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
