//! Console dashboard data collection (no terminal involved).

use tiered_memory::console::collect_state;
use tiered_memory::store::{LayeredDirStore, MemoryStore};
use tiered_memory::{Level, ProjectInfo, UserDb};

const USER: &str = "local";

fn seed(root: &std::path::Path) {
    let store = LayeredDirStore::new(root).unwrap();
    let mut db = UserDb::new("hashing:512".into(), 512);
    for (id, group) in [
        ("a", Some("rust-clis")),
        ("b", Some("rust-clis")),
        ("c", None),
    ] {
        db.projects.insert(
            id.into(),
            ProjectInfo {
                project_id: id.into(),
                name: id.into(),
                tags: vec![],
                components: vec![],
                descriptor: format!("project {id}"),
                descriptor_vector: vec![],
                similar: vec![],
                uses: vec![],
                group: group.map(str::to_string),
                created_at_ms: 1,
            },
        );
    }
    let mut style = crate_record(Level::L2, Some("a"), "commit messages are imperative");
    style.topic = Some("writing-style".into());
    db.records.push(style);
    let mut group_owned = crate_record(Level::L2, None, "all cli projects use clap");
    group_owned.group = Some("rust-clis".into());
    group_owned.topic = Some("tooling".into());
    db.records.push(group_owned);
    db.records
        .push(crate_record(Level::L2, Some("b"), "untagged l2"));
    db.records
        .push(crate_record(Level::L1, Some("a"), "hot line"));
    db.records
        .push(crate_record(Level::L3, None, "global trait"));
    store.save(USER, &db).unwrap();
}

#[allow(clippy::too_many_arguments)]
fn crate_record(level: Level, project: Option<&str>, text: &str) -> tiered_memory::MemoryRecord {
    tiered_memory::MemoryRecord {
        id: format!("m-{}", text.len()),
        user_id: USER.into(),
        text: text.into(),
        vector: vec![0.5],
        level,
        project_id: project.map(str::to_string),
        group: None,
        topic: None,
        kind: tiered_memory::MemoryKind::Note,
        params: Default::default(),
        key_hint: None,
        confidence: 0.8,
        pinned: false,
        created_at_ms: 1,
        last_used_at_ms: 1,
        use_count: 0,
        origin: None,
        expires_at_ms: None,
    }
}

#[test]
fn console_state_collects_layers_projects_groups_and_topics() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let cwd = dir.path().join("cwd");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    seed(dir.path());

    let state = collect_state(dir.path(), USER, &home, &cwd);
    assert_eq!(state.user, USER);
    assert_eq!(state.version, env!("CARGO_PKG_VERSION"));

    // layers
    assert_eq!(state.layers[0].count, 1, "one L1 record");
    assert_eq!(state.layers[1].count, 3, "three L2 records");
    assert_eq!(state.layers[2].count, 1, "one L3 record");
    assert_eq!(state.total_records(), 5);
    assert!(state.layers.iter().all(|l| l.capacity > 0));

    // projects + ungrouped
    assert_eq!(state.projects.len(), 3);
    assert_eq!(state.ungrouped, vec!["c".to_string()]);

    // groups: members + topic files (same bucketing as the mirrors)
    assert_eq!(state.groups.len(), 1);
    let g = &state.groups[0];
    assert_eq!(g.name, "rust-clis");
    assert_eq!(g.members, vec!["a".to_string(), "b".to_string()]);
    assert!(
        g.topics.contains(&("writing-style".to_string(), 1)),
        "topics: {:?}",
        g.topics
    );
    assert!(g.topics.contains(&("tooling".to_string(), 1)));
    assert!(
        g.topics.contains(&("general".to_string(), 1)),
        "untopic'd record lands in general.md: {:?}",
        g.topics
    );
}

#[test]
fn console_state_is_empty_but_valid_without_data() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let cwd = dir.path().join("cwd");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();

    let state = collect_state(dir.path(), USER, &home, &cwd);
    assert_eq!(state.total_records(), 0);
    assert!(state.projects.is_empty());
    assert!(state.groups.is_empty());
    assert_eq!(state.installs.len(), tiered_memory::harnesses::KNOWN.len());
    assert!(state.installs.iter().all(|i| !i.installed));
}
