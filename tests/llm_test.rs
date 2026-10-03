//! Tests for the LLM integration helpers: model-catalog fetching against a
//! mock OpenAI-compatible server.

#![cfg(feature = "server")]

use std::io::{Read, Write};
use std::net::TcpListener;
use tiered_memory::llm;

/// Mock HTTP server answering GET /models with `body`; returns its base URL.
fn spawn_mock_models(body: String) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 16384];
            let _ = stream.read(&mut buf);
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

#[test]
fn fetch_models_reads_openai_style_catalog() {
    let base = spawn_mock_models(
        r#"{"data": [{"id": "gpt-4o-mini"}, {"id": "gpt-4o"}, {"id": "o3-mini"}]}"#.into(),
    );
    let models = llm::fetch_models(&base, Some("sk-test")).unwrap();
    assert_eq!(models, vec!["gpt-4o", "gpt-4o-mini", "o3-mini"]);
}

#[test]
fn fetch_models_rejects_empty_catalog() {
    let base = spawn_mock_models(r#"{"data": []}"#.into());
    assert!(llm::fetch_models(&base, None).is_err());
}

#[test]
fn fetch_models_survives_local_server_shapes() {
    let base = spawn_mock_models(r#"{"models": ["llama3.2", {"name": "qwen2.5"}]}"#.into());
    let models = llm::fetch_models(&base, None).unwrap();
    assert_eq!(models, vec!["llama3.2", "qwen2.5"]);
}
