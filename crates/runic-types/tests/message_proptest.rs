//! Property tests for `Message` / `ContentBlock` JSON round-tripping — the
//! persistence + provider wire format. For ANY message, serialize → deserialize
//! → serialize must be identity (compared via `serde_json::Value`, since these
//! types don't derive `PartialEq`).

use proptest::prelude::*;

use runic_types::{ContentBlock, Message, Role};

fn json_value() -> impl Strategy<Value = serde_json::Value> {
    prop_oneof![
        Just(serde_json::Value::Null),
        any::<bool>().prop_map(serde_json::Value::from),
        (0i64..1000).prop_map(serde_json::Value::from),
        "[a-z ]{0,10}".prop_map(serde_json::Value::from),
    ]
}

fn opt_meta() -> impl Strategy<Value = Option<serde_json::Value>> {
    prop_oneof![
        Just(None),
        json_value().prop_map(|v| Some(serde_json::json!({ "k": v }))),
    ]
}

fn content_block() -> impl Strategy<Value = ContentBlock> {
    prop_oneof![
        ("[a-z ]{0,40}", opt_meta()).prop_map(|(text, provider_metadata)| ContentBlock::Text {
            text,
            provider_metadata,
        }),
        prop::sample::select(vec!["image/png", "image/jpeg", "image/webp"]).prop_map(|mt| {
            ContentBlock::Image { media_type: mt.into(), data: "YWJj".into() }
        }),
        ("[a-z]{1,8}", "[a-z_]{1,12}", json_value(), opt_meta()).prop_map(
            |(id, name, input, provider_metadata)| ContentBlock::ToolUse {
                id,
                name,
                input,
                provider_metadata,
            }
        ),
        ("[a-z]{1,8}", "[a-z_]{0,12}", "[a-z ]{0,30}", any::<bool>()).prop_map(
            |(tool_use_id, tool_name, content, is_error)| ContentBlock::ToolResult {
                tool_use_id,
                tool_name,
                content,
                is_error,
            }
        ),
        ("[a-z ]{0,30}", prop::option::of("[a-z0-9]{0,16}")).prop_map(|(thinking, signature)| {
            ContentBlock::Thinking { thinking, signature, provider_metadata: None }
        }),
        "[a-z0-9]{0,16}".prop_map(|data| ContentBlock::RedactedThinking { data }),
    ]
}

fn role() -> impl Strategy<Value = Role> {
    prop_oneof![Just(Role::User), Just(Role::Assistant), Just(Role::System)]
}

fn message() -> impl Strategy<Value = Message> {
    (role(), prop::collection::vec(content_block(), 0..6)).prop_map(|(role, blocks)| {
        let mut m = Message::user_with_blocks(blocks);
        m.role = role;
        m
    })
}

proptest! {
    /// A single content block round-trips through JSON unchanged.
    #[test]
    fn content_block_json_round_trips(b in content_block()) {
        let v = serde_json::to_value(&b).unwrap();
        let back: ContentBlock = serde_json::from_value(v.clone()).unwrap();
        prop_assert_eq!(serde_json::to_value(&back).unwrap(), v);
    }

    /// A full message round-trips through JSON unchanged.
    #[test]
    fn message_json_round_trips(m in message()) {
        let v = serde_json::to_value(&m).unwrap();
        let back: Message = serde_json::from_value(v.clone()).unwrap();
        prop_assert_eq!(serde_json::to_value(&back).unwrap(), v);
    }

    /// Every serialized block carries its `type` discriminator.
    #[test]
    fn content_block_is_type_tagged(b in content_block()) {
        let v = serde_json::to_value(&b).unwrap();
        prop_assert!(v.get("type").and_then(|t| t.as_str()).is_some(), "block missing `type`: {v}");
    }
}
