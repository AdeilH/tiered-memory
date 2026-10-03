//! End-to-end tour of the engine as a library (offline, hashing embedder):
//!
//! ```bash
//! cargo run --example basic
//! ```

use std::collections::BTreeMap;
use std::sync::Arc;
use tiered_memory::{
    EmbedderConfig, EngineConfig, FeedbackInput, JsonFileStore, MemoryEngine, ParamValue,
    ProjectInput, RecallInput, RememberInput,
};

fn main() -> tiered_memory::Result<()> {
    let data_dir = std::env::temp_dir().join("tiered-memory-demo");
    let store = Arc::new(JsonFileStore::new(&data_dir)?);
    let embedder = EmbedderConfig::default().build()?; // hashing:512, offline
    let engine = MemoryEngine::new(store, embedder, EngineConfig::default());
    let u = "learner-1";

    // -- register scopes -----------------------------------------------------
    // Same product, two components: they share one project id, so both L1s
    // demote into a shared L2. Similar projects link at registration too.
    for (id, desc) in [
        ("teacher-web", "teacher tutoring app react frontend chat ui"),
        ("teacher-api", "teacher tutoring app node backend llm api"),
        ("music-tutor", "music practice tutor app audio piano"),
    ] {
        engine.register_project(ProjectInput {
            user: u.into(),
            project_id: id.into(),
            name: Some(id.into()),
            tags: vec![],
            components: vec![],
            descriptor: Some(desc.into()),
            group: None,
        })?;
    }

    // -- global traits (L3) ---------------------------------------------------
    engine.feedback(FeedbackInput {
        user: u.into(),
        key: "analogy_domain".into(),
        value: ParamValue::Text("machines".into()),
        project_id: None,
        weight: Some(0.9),
        global: Some(true),
    })?;
    engine.remember(RememberInput {
        user: u.into(),
        text: "Learner is strong in Python, beginner in Rust systems topics".into(),
        kind: Some(tiered_memory::MemoryKind::Trait),
        params: {
            let mut m = BTreeMap::new();
            m.insert("difficulty".to_string(), ParamValue::Number(0.65));
            Some(m)
        },
        ..Default::default()
    })?;

    // -- project-local preferences (L1) --------------------------------------
    engine.feedback(FeedbackInput {
        user: u.into(),
        key: "difficulty".into(),
        value: ParamValue::Number(0.35),
        project_id: Some("teacher-web".into()),
        weight: None,
        global: None,
    })?;
    engine.remember(RememberInput {
        user: u.into(),
        text: "In this project the learner wants pure theory, no code examples".into(),
        project_id: Some("teacher-web".into()),
        params: {
            let mut m = BTreeMap::new();
            m.insert("code_example_density".to_string(), ParamValue::Number(0.0));
            Some(m)
        },
        ..Default::default()
    })?;

    // -- recall probes L1 -> L2 -> L3 ----------------------------------------
    let out = engine.recall(RecallInput {
        user: u.into(),
        query: "how should the tutor explain garbage collection?".into(),
        project_id: Some("teacher-web".into()),
        min_similarity: Some(0.02),
        ..Default::default()
    })?;
    println!("recall for teacher-web (searched {:?}):", out.searched);
    for h in &out.hits {
        println!("  [{:?}] sim={:.2} {}", h.level, h.similarity, h.text);
    }

    // -- adjusted parameters: defaults merged under learner adjustments ------
    let mut defaults = BTreeMap::new();
    defaults.insert("difficulty".to_string(), ParamValue::Number(0.5));
    defaults.insert(
        "lesson_style".to_string(),
        ParamValue::Text("standard".into()),
    );
    let merged = engine.parameters_with_defaults(u, Some("teacher-web"), &defaults)?;
    println!("\nadjusted parameters for teacher-web:");
    for (k, v) in &merged {
        println!("  {k} = {}", v.as_text());
    }

    // -- cross-project agreement lifts into L3 -------------------------------
    engine.feedback(FeedbackInput {
        user: u.into(),
        key: "pace".into(),
        value: ParamValue::Text("slow".into()),
        project_id: Some("teacher-web".into()),
        weight: None,
        global: None,
    })?;
    engine.feedback(FeedbackInput {
        user: u.into(),
        key: "pace".into(),
        value: ParamValue::Text("slow".into()),
        project_id: Some("teacher-api".into()),
        weight: None,
        global: None,
    })?;
    let report = engine.consolidate(u)?;
    println!(
        "\nconsolidation: merged={} traits_lifted={}",
        report.merged, report.traits_lifted
    );

    let params = engine.adjusted_parameters(u, Some("music-tutor"))?;
    println!("\nwhat a brand-new project (music-tutor) inherits:");
    for p in &params {
        println!("  {} = {} (from {:?})", p.key, p.value.as_text(), p.source);
    }

    println!("\ndata written to {}", data_dir.display());
    Ok(())
}
