#![cfg(feature = "cloud")]

use std::collections::HashMap;

use opendal::Operator;
use runic_skills::{SkillSet, source};

fn skill_md(name: &str, description: &str) -> String {
    format!("---\nname: {name}\ndescription: {description}\n---\nBody for {name}.")
}

fn operator(root: &std::path::Path) -> Operator {
    Operator::new(opendal::services::Fs::default().root(&root.to_string_lossy())).unwrap()
}

async fn seed(root: &std::path::Path) {
    for (path, body) in [
        ("deploy/SKILL.md", skill_md("deploy", "ship it")),
        ("deploy/checklist.md", "Pre-flight steps".to_string()),
        ("onboard/SKILL.md", skill_md("onboard", "welcome")),
        ("loose.txt", "not a skill".to_string()),
    ] {
        let target = root.join(path);
        tokio::fs::create_dir_all(target.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(target, body).await.unwrap();
    }
}

#[tokio::test]
async fn an_operator_backed_source_lists_folders_and_reads_files() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path()).await;
    let cloud = source::from_operator(operator(dir.path()));

    let mut entries = cloud.entries().await.unwrap();
    entries.sort();
    assert_eq!(
        entries,
        ["deploy", "onboard"],
        "only folders are skill entries; loose files are not"
    );

    assert!(
        cloud
            .read("deploy/SKILL.md")
            .await
            .unwrap()
            .contains("ship it")
    );
    assert_eq!(
        cloud.read("deploy/checklist.md").await.unwrap(),
        "Pre-flight steps"
    );
}

#[tokio::test]
async fn a_skillset_loads_through_an_operator_exactly_as_it_does_from_disk() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path()).await;

    let through_cloud = SkillSet::load(HashMap::from([(
        "acme".to_string(),
        source::from_operator(operator(dir.path())),
    )]))
    .await;
    let through_disk = SkillSet::load_dir("acme", dir.path()).await;

    let (mut cloud_ids, mut disk_ids) = (through_cloud.ids(), through_disk.ids());
    cloud_ids.sort();
    disk_ids.sort();

    assert_eq!(cloud_ids, ["acme:deploy", "acme:onboard"]);
    assert_eq!(
        cloud_ids, disk_ids,
        "the backend must not change what a SkillSet sees"
    );
}

#[tokio::test]
async fn traversal_is_refused_before_it_reaches_the_operator() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path()).await;
    let cloud = source::from_operator(operator(dir.path()));

    for escape in ["../secrets", "/etc/passwd", "deploy//SKILL.md", ""] {
        assert!(
            cloud.read(escape).await.is_err(),
            "{escape:?} should be refused"
        );
    }
}
