//! Slash-command expansion wired into the context engine.
//!
//! runic's command model (see `runic-commands`) is template expansion: a
//! `/report Dupont` invocation expands a `COMMAND.md` body into the user
//! message that reaches the model. The natural hook is the context
//! engine's [`process_user_input`](ContextEngine::process_user_input),
//! which runs on the incoming user turn.
//!
//! [`CommandExpansionEngine`] is a decorator: it owns a [`CommandRegistry`]
//! and an inner [`ContextEngine`] (usually a `CompositeEngine` carrying the
//! prompt layers). It intercepts `process_user_input` to expand a known
//! command and delegates every other method to the inner engine, so prompt
//! assembly / ambient notes / compaction are untouched.

use std::sync::Arc;

use async_trait::async_trait;
use runic_commands::{split_invocation, CommandRegistry};
use runic_context_engine::{AmbientNote, ContextEngine, TurnContext};
use runic_message_types::{ContentBlock, Message};

pub struct CommandExpansionEngine {
    inner: Arc<dyn ContextEngine>,
    commands: Arc<CommandRegistry>,
}

impl CommandExpansionEngine {
    pub fn new(inner: Arc<dyn ContextEngine>, commands: Arc<CommandRegistry>) -> Self {
        Self { inner, commands }
    }
}

/// First text block of a message, if any. Slash commands are plain text;
/// a multimodal message (image upload) isn't a command invocation.
fn first_text(msg: &Message) -> Option<&str> {
    msg.content.iter().find_map(|b| match b {
        ContentBlock::Text { text, .. } => Some(text.as_str()),
        _ => None,
    })
}

#[async_trait]
impl ContextEngine for CommandExpansionEngine {
    async fn assemble_system_prompt(&self, ctx: &TurnContext<'_>) -> String {
        self.inner.assemble_system_prompt(ctx).await
    }

    async fn ambient_notes(&self, ctx: &TurnContext<'_>) -> Vec<AmbientNote> {
        self.inner.ambient_notes(ctx).await
    }

    async fn maybe_compact(&self, ctx: &TurnContext<'_>, messages: &mut Vec<Message>) {
        self.inner.maybe_compact(ctx, messages).await;
    }

    async fn process_user_input(&self, ctx: &TurnContext<'_>, msg: Message) -> Message {
        // Only a single-text-block message can be a command; expand it in
        // place when the name resolves. Unknown `/foo` is left verbatim so
        // the model can react to it (and the chat shows what was typed).
        if let Some(text) = first_text(&msg)
            && let Some((name, args)) = split_invocation(text.trim())
            && let Some(cmd) = self.commands.get(name)
        {
            let expanded = cmd.expand(args);
            return Message {
                role: msg.role,
                content: vec![ContentBlock::Text {
                    text: expanded,
                    cache_control: None,
                }],
                timestamp: msg.timestamp,
                tool_duration_ms: msg.tool_duration_ms,
            };
        }
        self.inner.process_user_input(ctx, msg).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use runic_commands::Command;
    use runic_context_engine::NoopEngine;

    fn ctx<'a>() -> TurnContext<'a> {
        TurnContext {
            base_system_prompt: "",
            messages: &[],
            run_id: "r1",
            turn: 0,
            config: runic_context_engine::empty_config(),
        }
    }

    fn registry_with(cmd: &str) -> Arc<CommandRegistry> {
        let mut reg = CommandRegistry::new();
        reg.insert(Command::parse(cmd).unwrap());
        Arc::new(reg)
    }

    fn text_of(msg: &Message) -> String {
        first_text(msg).unwrap_or("").to_string()
    }

    #[tokio::test]
    async fn expands_known_command_with_args() {
        let reg = registry_with("---\nname: report\ndescription: d\n---\nPrépare un compte rendu pour : $ARGUMENTS");
        let engine = CommandExpansionEngine::new(Arc::new(NoopEngine), reg);

        let out = engine
            .process_user_input(&ctx(), Message::user("/report Dupont"))
            .await;
        assert_eq!(text_of(&out), "Prépare un compte rendu pour : Dupont");
    }

    #[tokio::test]
    async fn unknown_command_passes_through_verbatim() {
        let reg = registry_with("---\nname: report\ndescription: d\n---\nbody");
        let engine = CommandExpansionEngine::new(Arc::new(NoopEngine), reg);

        let out = engine
            .process_user_input(&ctx(), Message::user("/unknown stuff"))
            .await;
        assert_eq!(text_of(&out), "/unknown stuff");
    }

    #[tokio::test]
    async fn plain_text_is_left_untouched() {
        let reg = registry_with("---\nname: report\ndescription: d\n---\nbody");
        let engine = CommandExpansionEngine::new(Arc::new(NoopEngine), reg);

        let out = engine
            .process_user_input(&ctx(), Message::user("just a normal message"))
            .await;
        assert_eq!(text_of(&out), "just a normal message");
    }
}
