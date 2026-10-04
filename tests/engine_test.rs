//! Engine behavior tests. All run offline against the dependency-free
//! hashing embedder with an injected clock.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tiered_memory::{
    EngineConfig, FeedbackInput, ForgetInput, JsonFileStore, Level, MemoryEngine, MemoryError,
    MemoryStore, ParamValue, ProjectInput, RecallInput, RememberInput,
};

struct Clock(Arc<AtomicU64>);

impl Clock {
    fn new() -> Self {
        Clock(Arc::new(AtomicU64::new(1_700_000_000_000)))
    }
    fn advance_days(&self, d: f64) {
        self.0
            .fetch_add((d * 86_400_000.0) as u64, Ordering::Relaxed);
    }
    fn now_fn(&self) -> tiered_memory::NowFn {
        let t = self.0.clone();
        Arc::new(move || t.load(Ordering::Relaxed))
    }
}

fn engine_at(
    dir: &std::path::Path,
    clock: &Clock,
    tune: impl FnOnce(&mut EngineConfig),
) -> MemoryEngine {
    let mut cfg = EngineConfig::default();
    cfg.now = clock.now_fn();
    tune(&mut cfg);
    let store = Arc::new(JsonFileStore::new(dir).unwrap());
    let embedder = tiered_memory::EmbedderConfig::Hashing { dims: 512 }
        .build()
        .unwrap();
    MemoryEngine::new(store, embedder, cfg)
}

fn engine(dir: &std::path::Path) -> MemoryEngine {
    engine_at(dir, &Clock::new(), |_| {})
}

const U: &str = "u1";

fn register(engine: &MemoryEngine, project_id: &str, descriptor: &str) {
    engine
        .register_project(ProjectInput {
            user: U.into(),
            project_id: project_id.into(),
            name: Some(project_id.into()),
            tags: vec![],
            components: vec![],
            descriptor: Some(descriptor.into()),
            group: None,
        })
        .unwrap();
}

fn feedback(engine: &MemoryEngine, key: &str, value: ParamValue, project: Option<&str>) {
    engine
        .feedback(FeedbackInput {
            user: U.into(),
            key: key.into(),
            value,
            project_id: project.map(|p| p.into()),
            weight: None,
            global: Some(project.is_none()),
        })
        .unwrap();
}

fn recall(
    engine: &MemoryEngine,
    query: &str,
    project: Option<&str>,
) -> tiered_memory::RecallOutput {
    engine
        .recall(RecallInput {
            user: U.into(),
            query: query.into(),
            project_id: project.map(|p| p.into()),
            k: None,
            min_similarity: Some(0.05),
            write_allocate: None,
        })
        .unwrap()
}

#[test]
fn project_scoped_preference_overrides_global() {
    let dir = tempfile::tempdir().unwrap();
    let e = engine(dir.path());
    register(&e, "projA", "react frontend dashboard project");
    register(&e, "projB", "rust cli tooling project");

    feedback(&e, "difficulty", ParamValue::Number(0.7), None); // L3
    feedback(&e, "difficulty", ParamValue::Number(0.3), Some("projA")); // L1

    let a = e.adjusted_parameters(U, Some("projA")).unwrap();
    let diff = a.iter().find(|s| s.key == "difficulty").unwrap();
    assert_eq!(diff.value, ParamValue::Number(0.3));
    assert_eq!(diff.source, Level::L1);
    assert!(diff
        .alternatives
        .iter()
        .any(|alt| alt.value == ParamValue::Number(0.7) && alt.source == Level::L3));

    let b = e.adjusted_parameters(U, Some("projB")).unwrap();
    let diff_b = b.iter().find(|s| s.key == "difficulty").unwrap();
    assert_eq!(diff_b.value, ParamValue::Number(0.7));
    assert_eq!(diff_b.source, Level::L3);
}

#[test]
fn recall_serves_deep_layers_and_promotes_hot_ones() {
    let dir = tempfile::tempdir().unwrap();
    let e = engine_at(dir.path(), &Clock::new(), |c| c.promote_min_hits = 2);
    register(&e, "projA", "react frontend dashboard project");

    e.remember(RememberInput {
        user: U.into(),
        text: "Learner prefers analogies from games and play".into(),
        kind: Some(tiered_memory::MemoryKind::Trait),
        ..Default::default()
    })
    .unwrap();

    let first = recall(&e, "analogies from games and play", Some("projA"));
    assert!(first.hits.iter().all(|h| h.level != Level::L1));
    assert!(first.promoted.is_empty());

    // second recall crosses the promotion threshold
    let second = recall(&e, "analogies from games and play", Some("projA"));
    assert_eq!(
        second.promoted.len(),
        1,
        "expected write-allocate promotion"
    );

    // third recall serves the promoted L1 copy
    let third = recall(&e, "analogies from games and play", Some("projA"));
    assert!(
        third.hits.iter().any(|h| h.level == Level::L1),
        "hot memory should now serve from L1"
    );
}

#[test]
fn l1_eviction_writes_back_to_l2() {
    let dir = tempfile::tempdir().unwrap();
    let e = engine_at(dir.path(), &Clock::new(), |c| {
        c.l1_capacity = 1;
        c.dup_threshold = 0.999; // keep the two texts separate
    });
    register(&e, "projA", "react frontend dashboard project");

    e.remember(RememberInput {
        user: U.into(),
        text: "alpha beta gamma delta".into(),
        project_id: Some("projA".into()),
        ..Default::default()
    })
    .unwrap();
    e.remember(RememberInput {
        user: U.into(),
        text: "epsilon zeta eta theta".into(),
        project_id: Some("projA".into()),
        ..Default::default()
    })
    .unwrap();

    // L1 only holds one line; the other was demoted to L2, not lost.
    let stats = e.stats(U).unwrap();
    assert_eq!(stats.counts.l1, 1);
    assert_eq!(stats.counts.l2, 1);

    let out = recall(&e, "alpha beta gamma delta", Some("projA"));
    assert!(out
        .hits
        .iter()
        .any(|h| h.text.contains("alpha") && h.level == Level::L2));
}

#[test]
fn feedback_upserts_by_key_instead_of_duplicating() {
    let dir = tempfile::tempdir().unwrap();
    let e = engine(dir.path());
    register(&e, "projA", "react frontend project");

    feedback(&e, "difficulty", ParamValue::Number(0.5), Some("projA"));
    feedback(&e, "difficulty", ParamValue::Number(0.9), Some("projA"));

    let stats = e.stats(U).unwrap();
    assert_eq!(
        stats.counts.l1, 1,
        "same key at same scope must update, not duplicate"
    );
    let params = e.adjusted_parameters(U, Some("projA")).unwrap();
    assert_eq!(
        params.iter().find(|s| s.key == "difficulty").unwrap().value,
        ParamValue::Number(0.9)
    );
}

#[test]
fn ttl_expires_and_forget_removes() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Clock::new();
    let e = engine_at(dir.path(), &clock, |_| {});
    register(&e, "projA", "react frontend project");

    let out = e
        .remember(RememberInput {
            user: U.into(),
            text: "temporary note about this sprint".into(),
            project_id: Some("projA".into()),
            ttl_days: Some(2.0),
            ..Default::default()
        })
        .unwrap();

    clock.advance_days(3.0);
    let report = e.consolidate(U).unwrap();
    assert_eq!(report.expired, 1);

    // already gone — forgetting it again 404s
    let err = e.forget(ForgetInput {
        user: U.into(),
        id: Some(out.id),
        project_id: None,
        level: None,
        all: None,
    });
    assert!(matches!(err, Err(MemoryError::MemoryNotFound(_))));
}

#[test]
fn similar_projects_share_l2_memories() {
    let dir = tempfile::tempdir().unwrap();
    let e = engine(dir.path());
    register(
        &e,
        "shopfront",
        "portfolio website react frontend components styling",
    );
    register(
        &e,
        "shopadmin",
        "portfolio website react frontend dashboard components",
    );

    let projects = e.list_projects(U).unwrap();
    let a = projects
        .iter()
        .find(|p| p.project_id == "shopfront")
        .unwrap();
    assert!(
        a.similar.contains(&"shopadmin".to_string()),
        "descriptors should link"
    );

    e.remember(RememberInput {
        user: U.into(),
        text: "prefer zustand for state management".into(),
        project_id: Some("shopfront".into()),
        level: Some(Level::L2),
        ..Default::default()
    })
    .unwrap();

    let out = recall(&e, "prefer zustand for state management", Some("shopadmin"));
    assert!(
        out.hits
            .iter()
            .any(|h| h.text.contains("zustand") && h.level == Level::L2),
        "L2 memory of a similar project should surface"
    );
}

#[test]
fn l2_groups_share_memories_across_members_only() {
    let dir = tempfile::tempdir().unwrap();
    let e = engine(dir.path());
    register(&e, "projA", "rust cli argument parsing tooling");
    register(&e, "projB", "rust terminal utility application");
    register(&e, "projC", "react web dashboard interface");
    e.set_project_group(U, "projA", Some("rust-clis")).unwrap();
    e.set_project_group(U, "projB", Some("rust-clis")).unwrap();
    e.set_project_group(U, "projC", Some("web-apps")).unwrap();

    // project-owned L2 in projA — visible to group-mate projB, not to projC
    e.remember(RememberInput {
        user: U.into(),
        text: "prefer clap derive for argument parsing".into(),
        project_id: Some("projA".into()),
        level: Some(Level::L2),
        ..Default::default()
    })
    .unwrap();
    let b = recall(&e, "prefer clap derive argument parsing", Some("projB"));
    assert!(
        b.hits
            .iter()
            .any(|h| h.text.contains("clap") && h.level == Level::L2),
        "group-mates must see each other's project-owned L2 memories"
    );
    let c = recall(&e, "prefer clap derive argument parsing", Some("projC"));
    assert!(
        !c.hits.iter().any(|h| h.text.contains("clap")),
        "other groups must not see them"
    );

    // and in the other direction: projB's L2 surfaces to projA via the group
    e.remember(RememberInput {
        user: U.into(),
        text: "prefer ripgrep over grep for searching".into(),
        project_id: Some("projB".into()),
        level: Some(Level::L2),
        ..Default::default()
    })
    .unwrap();
    let a = recall(&e, "prefer ripgrep over grep searching", Some("projA"));
    assert!(a.hits.iter().any(|h| h.text.contains("ripgrep")));

    // group-owned record ("all my CLIs use clap"): no single owning project
    e.remember(RememberInput {
        user: U.into(),
        text: "all rust cli projects in this family use clap".into(),
        level: Some(Level::L2),
        group: Some("rust-clis".into()),
        ..Default::default()
    })
    .unwrap();
    for p in ["projA", "projB"] {
        let out = recall(&e, "all cli projects family use clap", Some(p));
        assert!(
            out.hits
                .iter()
                .any(|h| h.text.contains("family") && h.level == Level::L2),
            "group-owned memory must surface for member {p}"
        );
    }
    let hits = recall(&e, "all cli projects family use clap", Some("projC")).hits;
    assert!(!hits.iter().any(|h| h.text.contains("family")));

    // memory_context reports the group + members (the sync gather sees it)
    let ctx = e.memory_context(U, Some("projA"), 40).unwrap();
    assert_eq!(ctx.group.as_deref(), Some("rust-clis"));
    assert_eq!(ctx.group_members, vec!["projB".to_string()]);

    // `none` is a confirmation, not a group: projA leaving stops the sharing
    e.set_project_group(U, "projA", Some(tiered_memory::NO_GROUP))
        .unwrap();
    let ctx = e.memory_context(U, Some("projA"), 40).unwrap();
    assert_eq!(ctx.group, None, "`none` normalizes to no group");
    let a = recall(&e, "prefer ripgrep over grep searching", Some("projA"));
    assert!(
        !a.hits.iter().any(|h| h.text.contains("ripgrep")),
        "after leaving the group, group-mates' L2 must stop surfacing"
    );
    let a_own = recall(&e, "prefer clap derive argument parsing", Some("projA"));
    assert!(
        a_own.hits.iter().any(|h| h.text.contains("clap")),
        "a project always still sees its own L2 memories"
    );

    // L2 with neither project nor group is rejected instead of silently invisible
    let res = e.remember(RememberInput {
        user: U.into(),
        text: "orphan l2".into(),
        level: Some(Level::L2),
        ..Default::default()
    });
    assert!(matches!(res, Err(MemoryError::Invalid(_))));
}

#[test]
fn used_projects_share_l1_and_l2_memories() {
    let dir = tempfile::tempdir().unwrap();
    let e = engine(dir.path());
    register(&e, "projA", "react frontend project");
    register(&e, "projB", "rust systems project");

    // projA holds an L1 hot line, an L2 line, and an L1 parameter
    e.remember(RememberInput {
        user: U.into(),
        text: "prefers analogies from games".into(),
        project_id: Some("projA".into()),
        ..Default::default()
    })
    .unwrap();
    e.remember(RememberInput {
        user: U.into(),
        text: "uses zustand for state management".into(),
        project_id: Some("projA".into()),
        level: Some(Level::L2),
        ..Default::default()
    })
    .unwrap();
    feedback(&e, "difficulty", ParamValue::Number(0.2), Some("projA"));
    // and projB has a memory of its own so directionality is checkable
    e.remember(RememberInput {
        user: U.into(),
        text: "likes ripgrep over grep".into(),
        project_id: Some("projB".into()),
        ..Default::default()
    })
    .unwrap();

    // before the link: nothing of projA's surfaces in projB
    let before = recall(&e, "analogies from games", Some("projB"));
    assert!(
        !before.hits.iter().any(|h| h.text.contains("analogies")),
        "no borrowing without a `uses` link"
    );
    assert!(e
        .adjusted_parameters(U, Some("projB"))
        .unwrap()
        .iter()
        .all(|s| s.key != "difficulty"));

    e.add_project_use(U, "projB", "projA").unwrap();

    // after: projA's L1 and L2 surface in projB — the L1 line serving warm
    let out = recall(&e, "analogies from games", Some("projB"));
    let hot = out
        .hits
        .iter()
        .find(|h| h.text.contains("analogies"))
        .expect("used project's L1 memory must surface");
    assert_eq!(hot.level, Level::L2, "borrowed hot lines serve warm");
    assert_eq!(hot.project_id.as_deref(), Some("projA"));
    assert!(
        out.hits
            .iter()
            .any(|h| h.text.contains("zustand") && h.level == Level::L2),
        "used project's L2 memory must surface too"
    );

    // params: the borrowed L1 assertion ranks as L2 for projB
    let params = e.adjusted_parameters(U, Some("projB")).unwrap();
    let diff = params.iter().find(|s| s.key == "difficulty").unwrap();
    assert_eq!(diff.value, ParamValue::Number(0.2));
    assert_eq!(diff.source, Level::L2, "borrowed params come from warm tier");

    // …but projB's own L1 still beats the borrowed one (nearest layer wins)
    feedback(&e, "difficulty", ParamValue::Number(0.8), Some("projB"));
    let params = e.adjusted_parameters(U, Some("projB")).unwrap();
    let diff = params.iter().find(|s| s.key == "difficulty").unwrap();
    assert_eq!(diff.value, ParamValue::Number(0.8));
    assert_eq!(diff.source, Level::L1);
    assert!(diff
        .alternatives
        .iter()
        .any(|a| a.value == ParamValue::Number(0.2) && a.source == Level::L2));

    // directionality: projA does not see projB's memories
    let back = recall(&e, "ripgrep over grep", Some("projA"));
    assert!(
        !back.hits.iter().any(|h| h.text.contains("ripgrep")),
        "`uses` is directional — the used project gains nothing"
    );

    // memory_context buckets the borrowed line under L2 (the sync gather
    // treats it as warm context of projB, not as its hot line)
    let ctx = e.memory_context(U, Some("projB"), 40).unwrap();
    assert!(ctx.l1.iter().all(|l| !l.text.contains("analogies")));
    assert!(ctx.l2.iter().any(|l| l.text.contains("analogies")));

    // removing the link stops the sharing again
    e.remove_project_use(U, "projB", "projA").unwrap();
    let after = recall(&e, "analogies from games", Some("projB"));
    assert!(
        !after.hits.iter().any(|h| h.text.contains("analogies")),
        "borrowed lines must stop surfacing after unlinking"
    );

    // validation: no self-links, unknown projects 404
    let res = e.add_project_use(U, "projB", "projB");
    assert!(matches!(res, Err(MemoryError::Invalid(_))));
    let res = e.add_project_use(U, "projB", "ghost");
    assert!(matches!(res, Err(MemoryError::ProjectNotFound(_))));
}

#[test]
fn hot_borrowed_lines_promote_into_the_borrowers_l1() {
    let dir = tempfile::tempdir().unwrap();
    let e = engine_at(dir.path(), &Clock::new(), |c| c.promote_min_hits = 2);
    register(&e, "projA", "react frontend project");
    register(&e, "projB", "rust systems project");
    e.remember(RememberInput {
        user: U.into(),
        text: "prefers analogies from games".into(),
        project_id: Some("projA".into()),
        ..Default::default()
    })
    .unwrap();
    e.add_project_use(U, "projB", "projA").unwrap();

    // first recall touches; the second crosses the promotion threshold
    let first = recall(&e, "prefers analogies from games", Some("projB"));
    assert!(first.promoted.is_empty());
    let second = recall(&e, "prefers analogies from games", Some("projB"));
    assert_eq!(second.promoted.len(), 1, "borrowed hit promotes into L1");

    // the third recall serves the copy: a real L1 line of projB now
    let third = recall(&e, "prefers analogies from games", Some("projB"));
    let copy = third
        .hits
        .iter()
        .find(|h| h.level == Level::L1 && h.text.contains("analogies"))
        .expect("promoted copy serves from the borrower's hot line");
    assert_eq!(copy.project_id.as_deref(), Some("projB"));
}

#[test]
fn group_rename_moves_projects_and_group_owned_memories() {
    let dir = tempfile::tempdir().unwrap();
    let e = engine(dir.path());
    register(&e, "projA", "react frontend project");
    register(&e, "projB", "rust systems project");
    register(&e, "projC", "go services project");
    e.set_project_group(U, "projA", Some("typo-group")).unwrap();
    e.set_project_group(U, "projB", Some("typo-group")).unwrap();
    e.set_project_group(U, "projC", Some("other")).unwrap();

    // a group-owned memory carrying the typo'd name
    e.remember(RememberInput {
        user: U.into(),
        text: "all cli projects in this family use clap".into(),
        level: Some(Level::L2),
        group: Some("typo-group".into()),
        ..Default::default()
    })
    .unwrap();

    let (projects, records) = e.rename_group(U, "typo-group", "rust-clis").unwrap();
    assert_eq!((projects, records), (2, 1));

    let by_id = |id: &str| {
        e.list_projects(U)
            .unwrap()
            .into_iter()
            .find(|p| p.project_id == id)
            .unwrap()
    };
    assert_eq!(by_id("projA").group.as_deref(), Some("rust-clis"));
    assert_eq!(by_id("projB").group.as_deref(), Some("rust-clis"));
    assert_eq!(by_id("projC").group.as_deref(), Some("other"));

    // group-owned memories follow the rename
    let hits = recall(&e, "all cli projects family use clap", Some("projB")).hits;
    assert!(
        hits.iter().any(|h| h.text.contains("family")),
        "group-owned memory must stay visible after rename"
    );

    // renaming onto an existing group merges the two
    e.set_project_group(U, "projC", Some("other")).unwrap();
    let (projects, records) = e.rename_group(U, "rust-clis", "other").unwrap();
    assert_eq!((projects, records), (2, 1));
    assert_eq!(by_id("projA").group.as_deref(), Some("other"));

    // validation: same name, unknown source group, invalid target name
    assert!(matches!(
        e.rename_group(U, "other", "other"),
        Err(MemoryError::Invalid(_))
    ));
    assert!(matches!(
        e.rename_group(U, "ghost", "whatever"),
        Err(MemoryError::Invalid(_))
    ));
    assert!(matches!(
        e.rename_group(U, "other", "none"),
        Err(MemoryError::Invalid(_))
    ));
}

#[test]
fn uses_links_survive_re_registration_and_removal() {
    let dir = tempfile::tempdir().unwrap();
    let e = engine(dir.path());
    register(&e, "projA", "react frontend project");
    register(&e, "projB", "rust systems project");
    e.add_project_use(U, "projB", "projA").unwrap();

    // re-registering projB (like a re-init) keeps the user-set link
    register(&e, "projB", "rust systems project, embedded");
    let info = e
        .list_projects(U)
        .unwrap()
        .into_iter()
        .find(|p| p.project_id == "projB")
        .unwrap();
    assert_eq!(info.uses, vec!["projA".to_string()]);

    // removing projA strips the link from its users
    e.remove_project(U, "projA").unwrap();
    let info = e
        .list_projects(U)
        .unwrap()
        .into_iter()
        .find(|p| p.project_id == "projB")
        .unwrap();
    assert!(info.uses.is_empty(), "links to removed projects are dropped");
}

#[test]
fn topics_are_normalized_and_filed_into_per_topic_docs() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Clock::new();
    let store = Arc::new(tiered_memory::LayeredDirStore::new(dir.path()).unwrap());
    let embedder = tiered_memory::EmbedderConfig::Hashing { dims: 512 }
        .build()
        .unwrap();
    let mut cfg = EngineConfig::default();
    cfg.now = clock.now_fn();
    let e = MemoryEngine::new(store, embedder, cfg);

    e.register_project(ProjectInput {
        user: U.into(),
        project_id: "projA".into(),
        name: Some("projA".into()),
        tags: vec![],
        components: vec![],
        descriptor: Some("rust cli tooling project".into()),
        group: None,
    })
    .unwrap();
    e.set_project_group(U, "projA", Some("rust-clis")).unwrap();

    e.remember(RememberInput {
        user: U.into(),
        text: "keep commit messages concise and imperative".into(),
        project_id: Some("projA".into()),
        level: Some(Level::L2),
        topic: Some("Writing Style".into()),
        ..Default::default()
    })
    .unwrap();

    // the slug was normalized before persistence
    let loaded = tiered_memory::LayeredDirStore::new(dir.path())
        .unwrap()
        .load(U)
        .unwrap()
        .unwrap();
    let rec = loaded
        .records
        .iter()
        .find(|r| r.text.contains("commit"))
        .unwrap();
    assert_eq!(rec.topic.as_deref(), Some("writing-style"));

    // and the human-readable mirror is filed per group + topic
    let doc = std::fs::read_to_string(
        dir.path()
            .join("users/u1/cache/L2/groups/rust-clis/writing-style.md"),
    )
    .unwrap();
    assert!(doc.contains("commit messages"), "{doc}");
}

#[test]
fn zero_vector_embeddings_are_rejected_not_stored() {
    use std::sync::Arc as StdArc;
    use tiered_memory::{Embedder, Result as TmResult};

    // a broken embedder: everything comes back all-zero (failed model load,
    // empty feature extraction) — exactly what produced the junk `untitled`
    // project with a zero descriptor vector
    struct ZeroEmbedder;
    impl Embedder for ZeroEmbedder {
        fn name(&self) -> &'static str {
            "zero"
        }
        fn embed(&self, texts: &[String]) -> TmResult<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|t| vec![0.0; t.len().max(4)]).collect())
        }
        fn dims(&self) -> usize {
            4
        }
        fn default_min_similarity(&self) -> f32 {
            0.05
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let store: StdArc<dyn tiered_memory::MemoryStore> =
        Arc::new(JsonFileStore::new(dir.path()).unwrap());
    let e = MemoryEngine::new(store, StdArc::new(ZeroEmbedder), EngineConfig::default());

    // registration refuses instead of writing a junk project entry
    let res = e.register_project(ProjectInput {
        user: U.into(),
        project_id: "untitled".into(),
        name: Some("untitled".into()),
        tags: vec![],
        components: vec![],
        descriptor: Some("untitled".into()),
        group: None,
    });
    assert!(
        matches!(res, Err(MemoryError::Embedder(_))),
        "zero-vector descriptor must be rejected: {res:?}"
    );
    // and so must memories
    let res = e.remember(RememberInput {
        user: U.into(),
        text: "some memory".into(),
        ..Default::default()
    });
    assert!(matches!(res, Err(MemoryError::Embedder(_))));
    // the store stays clean
    let stats = e.stats(U).unwrap();
    assert_eq!(stats.projects, 0);
    assert_eq!(stats.counts.l1 + stats.counts.l2 + stats.counts.l3, 0);
}

#[test]
fn remove_project_drops_registry_records_and_links() {
    let dir = tempfile::tempdir().unwrap();
    let e = engine(dir.path());
    register(&e, "gone", "react frontend project");
    register(&e, "keeper", "react frontend dashboard");

    e.remember(RememberInput {
        user: U.into(),
        text: "hot line for gone".into(),
        project_id: Some("gone".into()),
        ..Default::default()
    })
    .unwrap();
    e.remember(RememberInput {
        user: U.into(),
        text: "l2 line for gone".into(),
        project_id: Some("gone".into()),
        level: Some(Level::L2),
        ..Default::default()
    })
    .unwrap();

    let removed = e.remove_project(U, "gone").unwrap();
    assert_eq!(removed, 2, "both records of the project are forgotten");

    let projects = e.list_projects(U).unwrap();
    assert!(projects.iter().all(|p| p.project_id != "gone"));
    // links pointing at the removed project are dropped from survivors
    let keeper = projects.iter().find(|p| p.project_id == "keeper").unwrap();
    assert!(!keeper.similar.contains(&"gone".to_string()));

    // unknown project 404s
    let res = e.remove_project(U, "gone");
    assert!(matches!(res, Err(MemoryError::ProjectNotFound(_))));
}

#[test]
fn consolidation_lifts_cross_project_traits_to_l3() {
    let dir = tempfile::tempdir().unwrap();
    let e = engine(dir.path());
    register(&e, "projA", "react frontend project");
    register(&e, "projB", "rust systems project");

    feedback(
        &e,
        "analogy_domain",
        ParamValue::Text("games".into()),
        Some("projA"),
    );
    feedback(
        &e,
        "analogy_domain",
        ParamValue::Text("games".into()),
        Some("projB"),
    );

    let report = e.consolidate(U).unwrap();
    assert_eq!(report.traits_lifted, 1);

    // the trait is now global — visible without any project context
    let params = e.adjusted_parameters(U, None).unwrap();
    let ad = params.iter().find(|s| s.key == "analogy_domain").unwrap();
    assert_eq!(ad.value, ParamValue::Text("games".into()));
    assert_eq!(ad.source, Level::L3);
}

#[test]
fn disagreeing_projects_are_not_lifted() {
    let dir = tempfile::tempdir().unwrap();
    let e = engine(dir.path());
    register(&e, "projA", "react frontend project");
    register(&e, "projB", "rust systems project");

    feedback(
        &e,
        "analogy_domain",
        ParamValue::Text("games".into()),
        Some("projA"),
    );
    feedback(
        &e,
        "analogy_domain",
        ParamValue::Text("nature".into()),
        Some("projB"),
    );

    let report = e.consolidate(U).unwrap();
    assert_eq!(report.traits_lifted, 0, "conflicts stay project-local");
}

#[test]
fn switching_embedder_requires_reindex() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Clock::new();
    let e512 = engine_at(dir.path(), &clock, |_| {});
    register(&e512, "projA", "react frontend project");
    e512.remember(RememberInput {
        user: U.into(),
        text: "likes worked examples".into(),
        project_id: Some("projA".into()),
        ..Default::default()
    })
    .unwrap();

    // same files, different embedder geometry: the store holds 512-dim vectors,
    // so a 256-dim engine must reindex before it can search
    let store256 = Arc::new(JsonFileStore::new(dir.path()).unwrap());
    let embedder256 = tiered_memory::EmbedderConfig::Hashing { dims: 256 }
        .build()
        .unwrap();
    let e256 = MemoryEngine::new(store256, embedder256, {
        let mut c = EngineConfig::default();
        c.now = clock.now_fn();
        c
    });
    e256.reindex(U).unwrap();
    let out = recall(&e256, "likes worked examples", Some("projA"));
    assert!(out.hits.iter().any(|h| h.text.contains("worked")));

    // a *fresh* engine still expecting the old 512-dim geometry must refuse
    let store = Arc::new(JsonFileStore::new(dir.path()).unwrap());
    let embedder = tiered_memory::EmbedderConfig::Hashing { dims: 512 }
        .build()
        .unwrap();
    let fresh = MemoryEngine::new(store, embedder, {
        let mut c = EngineConfig::default();
        c.now = clock.now_fn();
        c
    });
    let res = fresh.recall(RecallInput {
        user: U.into(),
        query: "anything".into(),
        project_id: None,
        k: None,
        min_similarity: None,
        write_allocate: None,
    });
    assert!(matches!(res, Err(MemoryError::EmbedderMismatch { .. })));
}

#[test]
fn pinned_memories_survive_capacity_pressure() {
    let dir = tempfile::tempdir().unwrap();
    let e = engine_at(dir.path(), &Clock::new(), |c| c.l1_capacity = 1);
    register(&e, "projA", "react frontend project");

    e.remember(RememberInput {
        user: U.into(),
        text: "pinned north star preference".into(),
        project_id: Some("projA".into()),
        pinned: Some(true),
        ..Default::default()
    })
    .unwrap();
    e.remember(RememberInput {
        user: U.into(),
        text: "ordinary filler memory".into(),
        project_id: Some("projA".into()),
        ..Default::default()
    })
    .unwrap();

    let stats = e.stats(U).unwrap();
    assert_eq!(
        stats.counts.l1, 1,
        "pinned line stays even under capacity pressure"
    );
    assert_eq!(
        stats.counts.l2, 1,
        "the unpinned line was demoted, not dropped"
    );
    let out = recall(&e, "pinned north star preference", Some("projA"));
    assert!(out
        .hits
        .iter()
        .any(|h| h.level == Level::L1 && h.text.contains("pinned")));
}

#[test]
fn parameters_with_defaults_merges_correctly() {
    let dir = tempfile::tempdir().unwrap();
    let e = engine(dir.path());
    register(&e, "projA", "react frontend project");
    feedback(&e, "difficulty", ParamValue::Number(0.3), Some("projA"));

    let mut defaults = std::collections::BTreeMap::new();
    defaults.insert("difficulty".to_string(), ParamValue::Number(0.5));
    defaults.insert(
        "lesson_style".to_string(),
        ParamValue::Text("standard".into()),
    );

    let merged = e
        .parameters_with_defaults(U, Some("projA"), &defaults)
        .unwrap();
    assert_eq!(merged["difficulty"], ParamValue::Number(0.3));
    assert_eq!(merged["lesson_style"], ParamValue::Text("standard".into()));
}
