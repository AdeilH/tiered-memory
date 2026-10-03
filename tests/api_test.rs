//! HTTP API round-trips via the router (no network, tower oneshot).

#![cfg(feature = "server")]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::sync::Arc;
use tiered_memory::{
    build_router, EmbedderConfig, EngineConfig, JsonFileStore, MemoryEngine, ServerState,
};
use tower::ServiceExt;

fn app_with_token(token: Option<&str>) -> axum::Router {
    let dir = tempfile::tempdir().unwrap();
    // NOTE: leak the tempdir so the store outlives the router (fine for tests)
    let root = dir.path().to_path_buf();
    std::mem::forget(dir);
    let store = Arc::new(JsonFileStore::new(root).unwrap());
    let embedder = EmbedderConfig::default().build().unwrap();
    let engine = Arc::new(MemoryEngine::new(store, embedder, EngineConfig::default()));
    build_router(Arc::new(ServerState {
        engine,
        token: token.map(str::to_string),
    }))
}

fn app() -> axum::Router {
    app_with_token(None)
}

async fn call(
    router: axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    let req = match body {
        Some(b) => req.body(Body::from(b.to_string())).unwrap(),
        None => req.body(Body::empty()).unwrap(),
    };
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

#[tokio::test]
async fn health_reports_embedder() {
    let (status, body) = call(app(), "GET", "/v1/health", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], json!(true));
    assert_eq!(body["embedder"], json!("hashing:512"));
}

#[tokio::test]
async fn token_auth_gates_routes_but_not_health() {
    let router = app_with_token(Some("sekrit"));

    let (status, _) = call(router.clone(), "GET", "/v1/health", None).await;
    assert_eq!(status, StatusCode::OK, "health stays open");

    let (status, body) = call(
        router.clone(),
        "POST",
        "/v1/remember",
        Some(json!({ "user": "u", "text": "x" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body["error"].as_str().unwrap().contains("token"));

    // authorized request passes
    let req = Request::builder()
        .method("POST")
        .uri("/v1/remember")
        .header("content-type", "application/json")
        .header("authorization", "Bearer sekrit")
        .body(Body::from(json!({"user":"u","text":"hello memory"}).to_string()))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn full_remember_recall_params_flow() {
    let router = app();
    let (status, _) = call(
        router.clone(),
        "POST",
        "/v1/projects",
        Some(json!({
            "user": "adeel",
            "project_id": "teacher",
            "name": "Teacher AI Skill Studio",
            "tags": ["tutoring", "llm"],
            "components": ["frontend", "backend"]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = call(
        router.clone(),
        "POST",
        "/v1/feedback",
        Some(json!({
            "user": "adeel",
            "project_id": "teacher",
            "key": "analogy_domain",
            "value": "games",
            "weight": 0.9
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["deduped"], json!(false));

    let (status, body) = call(
        router.clone(),
        "POST",
        "/v1/recall",
        Some(json!({
            "user": "adeel",
            "project_id": "teacher",
            "query": "analogy domain games",
            "min_similarity": 0.05
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body["hits"].as_array().unwrap().is_empty());

    let (status, body) = call(
        router.clone(),
        "POST",
        "/v1/params",
        Some(json!({
            "user": "adeel",
            "project_id": "teacher",
            "defaults": { "difficulty": 0.5, "lesson_style": "standard" }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["params"]["analogy_domain"], json!("games"));
    assert_eq!(body["params"]["difficulty"], json!(0.5));
    assert_eq!(body["detail"]["analogy_domain"]["source"], json!("L1"));
}

#[tokio::test]
async fn stats_and_forget() {
    let router = app();
    call(
        router.clone(),
        "POST",
        "/v1/remember",
        Some(json!({
            "user": "adeel",
            "text": "likes deep dives",
            "project_id": "teacher"
        })),
    )
    .await;

    let (_, body) = call(router.clone(), "GET", "/v1/stats/adeel", None).await;
    assert_eq!(body["counts"]["l1"], json!(1));
    assert_eq!(body["embedder"], json!("hashing:512"));

    let (status, body) = call(
        router.clone(),
        "POST",
        "/v1/forget",
        Some(json!({ "user": "adeel", "project_id": "teacher" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!(1));

    let (_, body) = call(router.clone(), "GET", "/v1/stats/adeel", None).await;
    assert_eq!(body["counts"]["l1"], json!(0));
}

#[tokio::test]
async fn bad_requests_map_to_4xx() {
    let router = app();
    let (status, body) = call(
        router,
        "POST",
        "/v1/remember",
        Some(json!({ "user": "adeel", "text": "" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("text"));
}
