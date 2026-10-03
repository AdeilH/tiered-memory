//! End-to-end sync pipeline test: gather (3 layers) → LLM extraction against
//! a mock OpenAI-compatible server → apply into the right layers.

#![cfg(feature = "server")]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use tiered_memory::{
    LayeredDirStore, LlmClient, LlmConfig, MemoryEngine, SyncInput, LOCAL_USER,
};

/// One-shot mock of POST /chat/completions that replies with a canned
/// assistant message (real HTTP, so the client's parsing is exercised too).
fn spawn_mock_llm(content: String) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 65536];
            let _ = stream.read(&mut buf); // request (headers + body) — content ignored
            let body = serde_json::json!({
                "choices": [
                    { "message": { "role": "assistant", "content": content } }
                ]
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    format!("http://{addr}")
}

fn engine() -> (MemoryEngine, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(LayeredDirStore::new(dir.path()).unwrap());
    let embedder = tiered_memory::EmbedderConfig::default().build().unwrap();
    (
        MemoryEngine::new(store, embedder, tiered_memory::EngineConfig::default()),
        dir,
    )
}

const PROJECT: &str = "teacher";

fn seed(engine: &MemoryEngine) {
    engine
        .register_project(tiered_memory::ProjectInput {
            user: LOCAL_USER.into(),
            project_id: PROJECT.into(),
            name: Some("Teacher".into()),
            tags: vec![],
            components: vec!["frontend".into(), "backend".into()],
            descriptor: Some("ai tutoring studio".into()),
        })
        .unwrap();
    // an existing global trait — the LLM must not re-assert it
    engine
        .remember(tiered_memory::RememberInput {
            user: LOCAL_USER.into(),
            text: "Learner is strong in Python".into(),
            kind: Some(tiered_memory::MemoryKind::Trait),
            ..Default::default()
        })
        .unwrap();
}

#[test]
fn sync_gathers_three_layers_extracts_and_applies() {
    let (engine, _dir) = engine();
    seed(&engine);

    // gather state must show L1 empty for the fresh project, L3 carrying the trait
    let ctx = engine.memory_context(LOCAL_USER, Some(PROJECT), 40).unwrap();
    assert!(ctx.l1.is_empty());
    assert!(ctx.l3.iter().any(|l| l.text.contains("Python")));

    // the mock model replies with a fenced-JSON extraction:
    // one L1 param update, one L3 trait, one bogus level (dropped)
    let content = serde_json::json!({
        "updates": [
            { "level": "L1", "text": "In this project the learner wants pure theory, no code",
              "key": "code_example_density", "params": { "code_example_density": 0.0 },
              "confidence": 0.9 },
            { "level": "L3", "text": "Learner is strong in Python",
              "confidence": 0.9 },
            { "level": "L9", "text": "bogus routing should be dropped silently" }
        ]
    });
    let content = format!("```json\n{content}\n```");
    let base = spawn_mock_llm(content);
    let llm = LlmClient::new(LlmConfig {
        base_url: base,
        api_key: Some("test-key".into()),
        model: "mock-1".into(),
        temperature: Some(0.0),
    })
    .unwrap();

    let input = SyncInput {
        user: LOCAL_USER.into(),
        project_id: PROJECT.into(),
        conversation: "Learner: I hate code snippets in this course, teach me pure theory. \
                       Also I know Python well."
            .into(),
    };
    let report = tiered_memory::sync(&engine, &llm, &input).unwrap();

    // both entries apply; the re-asserted L3 trait updates the existing
    // record in place (content dedupe) instead of duplicating it — asserted
    // by the layer counts below
    assert_eq!(report.stored.len(), 2, "L1 param update + L3 re-assertion apply");
    assert_eq!(report.stored[0].level, tiered_memory::Level::L1);
    assert_eq!(report.params_after["code_example_density"], tiered_memory::ParamValue::Number(0.0));

    // the entry landed at L1 of the project, not globally
    let stats = engine.stats(LOCAL_USER).unwrap();
    assert_eq!(stats.counts.l1, 1);
    assert_eq!(stats.counts.l3, 1, "existing trait still the only L3 record");

    // and the params view reflects it
    let params = engine
        .parameters_with_defaults(LOCAL_USER, Some(PROJECT), &Default::default())
        .unwrap();
    assert_eq!(params["code_example_density"], tiered_memory::ParamValue::Number(0.0));
}

#[test]
fn sync_with_nothing_new_stores_nothing() {
    let (engine, _dir) = engine();
    seed(&engine);

    let base = spawn_mock_llm("{\"updates\": []}".into());
    let llm = LlmClient::new(LlmConfig {
        base_url: base,
        api_key: None,
        model: "mock-1".into(),
        temperature: None,
    })
    .unwrap();

    let input = SyncInput {
        user: LOCAL_USER.into(),
        project_id: PROJECT.into(),
        conversation: "Learner: nothing memorable happened today".into(),
    };
    let report = tiered_memory::sync(&engine, &llm, &input).unwrap();
    assert!(report.stored.is_empty());
    assert!(report.params_after.is_empty() || report.params_after.len() == 1);
}
