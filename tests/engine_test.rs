//! Engine behavior tests. All run offline against the dependency-free
//! hashing embedder with an injected clock.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tiered_memory::{
    EngineConfig, FeedbackInput, ForgetInput, JsonFileStore, Level, MemoryEngine, MemoryError,
    ParamValue, ProjectInput, RecallInput, RememberInput,
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
