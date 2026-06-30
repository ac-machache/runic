//! The `App` component — sidebar (connection + threads), chat pane, and the
//! inspector (Events / State). Talks to the server over HTTP + SSE; events are
//! parsed leniently as JSON so the UI never hard-couples to the server's
//! internal `WireEvent` type.

use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::Value;
use wasm_bindgen::{JsCast, JsValue, closure::Closure};
use wasm_bindgen_futures::JsFuture;

use crate::api::ApiClient;
use crate::events::{
    append_live, apply_event, cluster_runs, flush_live, items_from_events, parse_ask, usage_of,
};
use crate::model::{Attachment, Item, LiveBuf, LiveKind, PendingAsk, ThreadInfo};
use crate::util::short_id;
use crate::views::{render_item, render_run, render_state};

#[component]
pub fn App() -> impl IntoView {
    // ── connection / identity ──────────────────────────────────────────
    let api_base = RwSignal::new("http://127.0.0.1:8920".to_string());
    let tenant = RwSignal::new("default".to_string());

    // ── thread + transcript state ──────────────────────────────────────
    let threads = RwSignal::new(Vec::<ThreadInfo>::new());
    let threads_cursor = RwSignal::new(None::<String>);
    let current = RwSignal::new(None::<String>);
    let items = RwSignal::new(Vec::<Item>::new());
    let live = RwSignal::new(LiveBuf::default());
    let events = RwSignal::new(Vec::<Value>::new());
    let usage = RwSignal::new(None::<(u64, u64)>);
    let input = RwSignal::new(String::new());
    let attachments = RwSignal::new(Vec::<Attachment>::new());
    let uploading = RwSignal::new(0usize);
    let streaming = RwSignal::new(false);
    let context_json = RwSignal::new(String::new());
    let pending = RwSignal::new(None::<PendingAsk>);
    let ask_answer = RwSignal::new(String::new());
    let has_pending = Memo::new(move |_| pending.get().is_some());

    let abort = RwSignal::new(None::<web_sys::AbortController>);
    let inspect_tab = RwSignal::new("events");
    let state_json = RwSignal::new(None::<Value>);

    // ── chrome / UI state ──────────────────────────────────────────────
    let dark = RwSignal::new(true);
    let collapsed = RwSignal::new(false);
    let config_open = RwSignal::new(false);
    let split = RwSignal::new(50.0_f64); // chat % of the main area
    let dragging = RwSignal::new(false);
    let show_thinking = RwSignal::new(false);
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
            match c.list_threads(None).await {
                Ok((ts, cursor)) => {
                    threads.set(ts);
                    threads_cursor.set(cursor);
                }
                Err(e) => leptos::logging::warn!("list_threads failed: {e}"),
            }
        });
    };

    let load_more_threads = move || {
        let Some(cursor) = threads_cursor.get_untracked() else {
            return;
        };
        let c = client();
        spawn_local(async move {
            match c.list_threads(Some(&cursor)).await {
                Ok((mut more, next)) => {
                    threads.update(|t| t.append(&mut more));
                    threads_cursor.set(next);
                }
                Err(e) => leptos::logging::warn!("list_threads page failed: {e}"),
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
                    match c.list_threads(None).await {
                        Ok((ts, cursor)) => {
                            threads.set(ts);
                            threads_cursor.set(cursor);
                        }
                        Err(_) => threads.update(|t| {
                            if !t.iter().any(|x| x.id == id) {
                                t.push(ThreadInfo {
                                    id: id.clone(),
                                    label: None,
                                })
                            }
                        }),
                    }
                }
                Err(e) => leptos::logging::warn!("create_thread failed: {e}"),
            }
        });
    };

    let send = move || {
        let text = input.get_untracked();
        let atts = attachments.get_untracked();
        if (text.trim().is_empty() && atts.is_empty())
            || streaming.get_untracked()
            || uploading.get_untracked() > 0
        {
            return;
        }
        let c = client();
        let thread_id = match current.get_untracked() {
            Some(id) => id,
            None => return,
        };
        let context_text = context_json.get_untracked();
        let context_val: Option<Value> = if context_text.trim().is_empty() {
            None
        } else {
            match serde_json::from_str::<Value>(&context_text) {
                Ok(v) => Some(v),
                Err(e) => {
                    items.update(|its| {
                        its.push(Item::Warning(format!("invalid context JSON: {e}")))
                    });
                    return;
                }
            }
        };

        input.set(String::new());
        attachments.set(Vec::new());
        let display = if atts.is_empty() {
            text.clone()
        } else {
            let names = atts
                .iter()
                .map(|a| a.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            format!("{text}\n📎 {names}")
        };
        items.update(|its| its.push(Item::User(display)));
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
                    "ask_required" => {
                        flush_live(live, items);
                        if let Some(p) = parse_ask(&ev) {
                            pending.set(Some(p));
                        }
                    }
                    _ => {
                        flush_live(live, items);
                        items.update(|its| apply_event(its, &ev));
                    }
                }
            };
            let result = c
                .stream_run(
                    &thread_id,
                    &text,
                    &atts,
                    context_val,
                    signal.as_ref(),
                    on_event,
                )
                .await;
            flush_live(live, items);
            if let Err(e) = result {
                if !e.to_lowercase().contains("abort") {
                    items.update(|its| its.push(Item::Warning(format!("stream error: {e}"))));
                } else {
                    items.update(|its| its.push(Item::Warning("— stopped —".into())));
                }
            }
            match c.list_threads(None).await {
                Ok((ts, cursor)) => {
                    threads.set(ts);
                    threads_cursor.set(cursor);
                }
                Err(e) => leptos::logging::warn!("list_threads refresh failed: {e}"),
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

    let file_input_ref = NodeRef::<leptos::html::Input>::new();
    let pick_files = move || {
        let Some(el) = file_input_ref.get() else {
            return;
        };
        let Some(files) = el.files() else {
            return;
        };
        let Some(thread_id) = current.get_untracked() else {
            leptos::logging::warn!("pick or create a thread before attaching files");
            return;
        };
        for i in 0..files.length() {
            let Some(file) = files.get(i) else { continue };
            let name = file.name();
            let mut media_type = file.type_();
            if media_type.is_empty() {
                media_type = "application/octet-stream".to_string();
            }
            let c = client();
            let thread_id = thread_id.clone();
            uploading.update(|n| *n += 1);
            if media_type.starts_with("audio/") {
                // Audio is preprocessed to text and sent — never stored or attached.
                spawn_local(async move {
                    let gfile = gloo_file::File::from(file);
                    let result = match gloo_file::futures::read_as_bytes(&gfile).await {
                        Ok(bytes) => c.transcribe(bytes, &media_type, &name).await,
                        Err(e) => Err(format!("read file failed: {e:?}")),
                    };
                    uploading.update(|n| *n = n.saturating_sub(1));
                    match result {
                        Ok(text) => {
                            input.set(text);
                            send();
                        }
                        Err(e) => leptos::logging::warn!("transcribe failed: {e}"),
                    }
                });
            } else {
                // Everything else uploads to the artifact store; the message carries the id.
                spawn_local(async move {
                    let gfile = gloo_file::File::from(file);
                    match gloo_file::futures::read_as_bytes(&gfile).await {
                        Ok(bytes) => match c
                            .upload_artifact(&thread_id, bytes, &media_type, &name)
                            .await
                        {
                            Ok(att) => attachments.update(|a| a.push(att)),
                            Err(e) => leptos::logging::warn!("upload failed: {e}"),
                        },
                        Err(e) => leptos::logging::warn!("read file failed: {e:?}"),
                    }
                    uploading.update(|n| *n = n.saturating_sub(1));
                });
            }
        }
        // Reset so re-selecting the same file fires `change` again.
        el.set_value("");
    };

    // ── voice recording ────────────────────────────────────────────────
    let recording = RwSignal::new(false);
    let recorder = RwSignal::new(None::<(web_sys::MediaRecorder, web_sys::MediaStream)>);
    let toggle_record = move || {
        if recording.get_untracked() {
            // Stop: triggers `ondataavailable` (which transcribes + sends), and
            // release the mic.
            if let Some((rec, stream)) = recorder.get_untracked() {
                let _ = rec.stop();
                let tracks = stream.get_tracks();
                for i in 0..tracks.length() {
                    if let Ok(track) = tracks.get(i).dyn_into::<web_sys::MediaStreamTrack>() {
                        track.stop();
                    }
                }
            }
            recorder.set(None);
            recording.set(false);
            return;
        }
        if current.get_untracked().is_none() {
            leptos::logging::warn!("pick or create a thread before recording");
            return;
        }
        let c = client();
        spawn_local(async move {
            let Some(md) = web_sys::window().and_then(|w| w.navigator().media_devices().ok())
            else {
                leptos::logging::warn!("no media devices");
                return;
            };
            let constraints = web_sys::MediaStreamConstraints::new();
            constraints.set_audio(&JsValue::TRUE);
            let stream = match md.get_user_media_with_constraints(&constraints) {
                Ok(p) => match JsFuture::from(p).await {
                    Ok(s) => s.unchecked_into::<web_sys::MediaStream>(),
                    Err(_) => {
                        leptos::logging::warn!("microphone permission denied");
                        return;
                    }
                },
                Err(_) => return,
            };
            let Ok(rec) = web_sys::MediaRecorder::new_with_media_stream(&stream) else {
                leptos::logging::warn!("MediaRecorder unavailable");
                return;
            };
            // One blob on stop → transcribe → auto-send (same path as a file).
            let on_data =
                Closure::<dyn FnMut(web_sys::BlobEvent)>::new(move |e: web_sys::BlobEvent| {
                    let Some(blob) = e.data() else { return };
                    let c = c.clone();
                    uploading.update(|n| *n += 1);
                    spawn_local(async move {
                        // The mic records webm; the chat-audio model needs wav, so
                        // decode + re-encode in the browser before sending.
                        let gblob = gloo_file::Blob::from(blob);
                        let result = match gloo_file::futures::read_as_bytes(&gblob).await {
                            Ok(webm) => match webm_to_wav(webm).await {
                                Ok(wav) => c.transcribe(wav, "audio/wav", "recording.wav").await,
                                Err(e) => Err(e),
                            },
                            Err(e) => Err(format!("read recording failed: {e:?}")),
                        };
                        uploading.update(|n| *n = n.saturating_sub(1));
                        match result {
                            Ok(text) => {
                                input.set(text);
                                send();
                            }
                            Err(e) => leptos::logging::error!("recording transcribe failed: {e}"),
                        }
                    });
                });
            rec.set_ondataavailable(Some(on_data.as_ref().unchecked_ref()));
            on_data.forget();
            if rec.start().is_err() {
                leptos::logging::warn!("could not start recording");
                return;
            }
            recorder.set(Some((rec, stream)));
            recording.set(true);
        });
    };

    let fetch_state = move || {
        let Some(id) = current.get_untracked() else {
            return;
        };
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
                            <img class="brand-mark" src="favicon.png" alt="runic" />
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
                        {move || threads.get().into_iter().map(|t| {
                            let id = t.id;
                            let id_active = id.clone();
                            let id_click = id.clone();
                            let short = short_id(&id);
                            let untitled = t.label.is_none();
                            let title = t.label.unwrap_or_else(|| "untitled".to_string());
                            view! {
                                <div class="thread"
                                    class:active=move || current.get().as_deref() == Some(id_active.as_str())
                                    on:click=move |_| load_thread(id_click.clone())>
                                    <span class="thread-accent"></span>
                                    <div class="thread-row1">
                                        <span class="thread-id">{short}</span>
                                    </div>
                                    <div class="thread-title" class:untitled=untitled>{title}</div>
                                </div>
                            }
                        }).collect_view()}
                        {move || threads_cursor.get().map(|_| view! {
                            <button class="newthread" on:click=move |_| load_more_threads()>"Load more"</button>
                        })}
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
                                            <img class="avatar" src="favicon.png" alt="runic" />
                                            <div class="assistant-body"><div class="thinking-body">{lb.text}</div></div>
                                        </div>
                                    }.into_any()
                                } else {
                                    view! {
                                        <div class="msg-assistant">
                                            <img class="avatar" src="favicon.png" alt="runic" />
                                            <div class="assistant-body">
                                                <div class="prose"><p style="margin:0">{lb.text}<span class="caret"></span></p></div>
                                            </div>
                                        </div>
                                    }.into_any()
                                }
                            }}
                        </div>
                    </div>

                    // HITL ask card (the agent called ask_user)
                    {move || has_pending.get().then(|| {
                        let p = pending.get_untracked().expect("has_pending implies Some");
                        let send_answer = move |_| {
                            let Some(p) = pending.get_untracked() else { return };
                            let a = ask_answer.get_untracked();
                            if a.trim().is_empty() { return; }
                            let c = client();
                            let thread = current.get_untracked().unwrap_or_default();
                            let ask_id = p.ask_id.clone();
                            pending.set(None);
                            ask_answer.set(String::new());
                            spawn_local(async move { let _ = c.submit_answer(&thread, &ask_id, a).await; });
                        };
                        view! {
                            <div class="composer">
                                <div class="composer-inner">
                                    <div class="approval">
                                        <div class="apv-head">
                                            <span class="ic">"⏸"</span>
                                            <span class="apv-name">"agent is asking"</span>
                                            <span class="apv-badge">"input needed"</span>
                                        </div>
                                        <div class="apv-body">
                                            <div class="apv-summary">{p.question.clone()}</div>
                                            {p.context.clone().map(|c| view! { <div class="apv-ctx">{c}</div> })}
                                            <textarea class="apv-input" spellcheck="false" placeholder="Your answer…"
                                                prop:value=move || ask_answer.get()
                                                on:input=move |e| ask_answer.set(event_target_value(&e))></textarea>
                                            <div class="apv-actions">
                                                <button class="apv-submit" on:click=send_answer>"Send answer"</button>
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
                                    </div>
                                </div>
                            })}

                            {move || {
                                let a = attachments.get();
                                (!a.is_empty()).then(|| view! {
                                    <div class="attach-row">
                                        {a.into_iter().enumerate().map(|(i, att)| view! {
                                            <span class="attach-chip" title=att.media_type.clone()>
                                                <span class="attach-name">{att.name}</span>
                                                <button class="attach-x" title="Remove"
                                                    on:click=move |_| attachments.update(|v| { if i < v.len() { v.remove(i); } })>"✕"</button>
                                            </span>
                                        }).collect_view()}
                                    </div>
                                })
                            }}

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
                                <button class="gear-btn" title="Attach files"
                                    on:click=move |_| { if let Some(el) = file_input_ref.get() { el.click(); } }
                                    prop:disabled=move || current.get().is_none() || streaming.get()>"📎"</button>
                                <input type="file" multiple node_ref=file_input_ref style="display:none"
                                    on:change=move |_| pick_files() />
                                <button class="gear-btn" class:recording=move || recording.get()
                                    title=move || if recording.get() { "Stop recording" } else { "Record voice" }
                                    on:click=move |_| toggle_record()
                                    prop:disabled=move || current.get().is_none() || streaming.get()>
                                    {move || if recording.get() { "⏹" } else { "🎤" }}
                                </button>
                                <button class="gear-btn" title="Configurable" on:click=move |_| config_open.update(|c| *c = !*c)>
                                    "⚙"
                                    {move || (!context_json.get().trim().is_empty()).then(|| view! { <span class="gear-dot"></span> })}
                                </button>
                                {move || if streaming.get() {
                                    view! { <button class="send-btn stop" on:click=move |_| stop()>"◼ Stop"</button> }.into_any()
                                } else {
                                    view! {
                                        <button class="send-btn" on:click=move |_| send()
                                            prop:disabled=move || current.get().is_none() || (uploading.get() > 0)>
                                            {move || if uploading.get() > 0 { "Uploading…" } else { "Send ↵" }}
                                        </button>
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
                                Some(s) => render_state(&s, fetch_state).into_any(),
                            }
                        })}
                    </div>
                </section>
            </div>
        </div>
    }
}

/// Decode a recorded audio blob (webm/opus etc.) via Web Audio and re-encode it
/// as 16-bit PCM WAV — the format the Mistral chat-audio endpoint accepts.
async fn webm_to_wav(bytes: Vec<u8>) -> Result<Vec<u8>, String> {
    let ctx = web_sys::AudioContext::new().map_err(|e| format!("AudioContext init: {e:?}"))?;
    let buffer = js_sys::Uint8Array::from(bytes.as_slice()).buffer();
    let promise = ctx
        .decode_audio_data(&buffer)
        .map_err(|e| format!("decode_audio_data: {e:?}"))?;
    let decoded = JsFuture::from(promise)
        .await
        .map_err(|e| format!("could not decode recording (unsupported audio?): {e:?}"))?;
    let _ = ctx.close();

    let audio: web_sys::AudioBuffer = decoded
        .dyn_into()
        .map_err(|_| "decoded value is not an AudioBuffer".to_string())?;
    let channels = audio.number_of_channels() as usize;
    let frames = audio.length() as usize;
    let sample_rate = audio.sample_rate() as u32;
    if channels == 0 || frames == 0 {
        return Err("empty recording".into());
    }

    let mut chans = Vec::with_capacity(channels);
    for c in 0..channels {
        chans.push(
            audio
                .get_channel_data(c as u32)
                .map_err(|e| format!("get_channel_data({c}): {e:?}"))?,
        );
    }
    let mut pcm: Vec<i16> = Vec::with_capacity(frames * channels);
    for f in 0..frames {
        for ch in &chans {
            let s = ch.get(f).copied().unwrap_or(0.0).clamp(-1.0, 1.0);
            pcm.push((s * 32767.0) as i16);
        }
    }
    Ok(wav_bytes(channels as u16, sample_rate, &pcm))
}

/// Wrap interleaved 16-bit PCM in a canonical 44-byte WAV header.
fn wav_bytes(channels: u16, sample_rate: u32, pcm: &[i16]) -> Vec<u8> {
    let block_align = channels * 2;
    let byte_rate = sample_rate * block_align as u32;
    let data_len = (pcm.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes()); // bits/sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in pcm {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}
