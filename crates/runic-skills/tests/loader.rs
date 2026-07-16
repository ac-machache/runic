//! End-to-end tests over the real `LocalSource` (tokio::fs): loading, the
//! safety checks, and the `skill_view` tool incl. sub-file traversal refusal.

use std::collections::HashMap;
use std::sync::Arc;

use runic_skills::{SkillSet, source};
use runic_tool::ToolContext;

async fn write(root: &std::path::Path, rel: &str, body: &str) {
    let path = root.join(rel);
    tokio::fs::create_dir_all(path.parent().unwrap())
        .await
        .unwrap();
    tokio::fs::write(path, body).await.unwrap();
}

#[tokio::test]
async fn load_dir_loads_valid_and_skips_the_rest() {
    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "valid/SKILL.md",
        "---\nname: valid\ndescription: works\n---\nFull instructions.",
    )
    .await;
    // no description -> dropped (description is required)
    write(
        root.path(),
        "nodesc/SKILL.md",
        "---\nname: nodesc\ndescription: \n---\nBody.",
    )
    .await;
    // no closing frontmatter -> dropped
    write(
        root.path(),
        "broken/SKILL.md",
        "---\nname: broken\nno terminator",
    )
    .await;
    // a folder without SKILL.md, a dotfile dir, and a loose top-level file -> ignored
    tokio::fs::create_dir_all(root.path().join("plain-dir"))
        .await
        .unwrap();
    tokio::fs::create_dir_all(root.path().join(".hidden"))
        .await
        .unwrap();
    write(
        root.path(),
        ".hidden/SKILL.md",
        "---\nname: h\ndescription: d\n---\nx",
    )
    .await;
    tokio::fs::write(root.path().join("top-level.md"), "ignored")
        .await
        .unwrap();

    let set = SkillSet::load_dir("core", root.path()).await;

    assert_eq!(set.ids(), vec!["core:valid"]);
    assert_eq!(set.get("core:valid").unwrap().body, "Full instructions.");
}

#[tokio::test]
async fn missing_dir_yields_empty_set_not_error() {
    let missing = tempfile::tempdir().unwrap().path().join("missing");
    let set = SkillSet::load_dir("core", missing).await;
    assert!(set.is_empty());
}

#[tokio::test]
async fn load_merges_multiple_namespaced_sources() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    write(
        a.path(),
        "alpha/SKILL.md",
        "---\nname: alpha\ndescription: first\n---\nA.",
    )
    .await;
    write(
        b.path(),
        "beta/SKILL.md",
        "---\nname: beta\ndescription: second\n---\nB.",
    )
    .await;

    let set = SkillSet::load(HashMap::from([
        ("one".to_string(), source::local(a.path())),
        ("two".to_string(), source::local(b.path())),
    ]))
    .await;

    let mut ids = set.ids();
    ids.sort();
    assert_eq!(ids, vec!["one:alpha", "two:beta"]);
}

#[tokio::test]
async fn skill_view_reads_body_subfile_and_refuses_traversal() {
    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "alpha/SKILL.md",
        "---\nname: alpha\ndescription: first\n---\nAlpha body.",
    )
    .await;
    write(root.path(), "alpha/references/note.md", "the note").await;

    let set = Arc::new(SkillSet::load_dir("core", root.path()).await);
    let tool = set.view_tool().expect("non-empty set has a view tool");
    let ctx = ToolContext::new("u", "s", "r");

    // body
    let r = tool
        .execute(serde_json::json!({ "name": "core:alpha" }), &ctx)
        .await
        .unwrap();
    assert!(!r.is_error() && r.text().contains("Alpha body."));

    // sub-file through the source
    let r = tool
        .execute(
            serde_json::json!({ "name": "core:alpha", "path": "references/note.md" }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(!r.is_error() && r.text().contains("the note"));

    // traversal refused
    let r = tool
        .execute(
            serde_json::json!({ "name": "core:alpha", "path": "../../etc/passwd" }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(r.is_error());

    // unknown skill
    let r = tool
        .execute(serde_json::json!({ "name": "core:ghost" }), &ctx)
        .await
        .unwrap();
    assert!(r.is_error());
}

#[tokio::test]
async fn empty_set_has_no_view_tool() {
    let empty = tempfile::tempdir().unwrap();
    let set = Arc::new(SkillSet::load_dir("core", empty.path()).await);
    assert!(set.view_tool().is_none());
}
