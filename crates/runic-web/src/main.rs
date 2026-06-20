//! runic dev console — a Leptos CSR app for driving `runic serve`.
//!
//! Layout: a collapsible sidebar (connection + threads), then a chat pane and
//! an inspector pane split 50/50 by a draggable splitter. The inspector has two
//! tabs: **Events** (the run/turn-clustered activity tree) and **State**
//! (system prompt + messages as sent to the model). Talks to the server over
//! HTTP + SSE; events are parsed leniently as JSON so the UI never hard-couples
//! to the server's internal `WireEvent` type.
//!
//! Modules:
//! - [`api`]    — HTTP/SSE client for `runic serve`.
//! - [`model`]  — plain UI data types.
//! - [`util`]   — small pure helpers.
//! - [`events`] — folding the event stream into the UI model (+ tests).
//! - [`views`]  — render helpers.
//! - [`app`]    — the `App` component.

mod api;
mod app;
mod events;
mod model;
mod util;
mod views;

fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(app::App);
}
