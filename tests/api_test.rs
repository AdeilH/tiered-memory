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
    // hashing, not Default: tests must stay offline even with `--features local`
    let embedder = EmbedderConfig::Hashing { dims: 512 }.build().unwrap();
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
        .body(Body::from(
            json!({"user":"u","text":"hello memory"}).to_string(),
        ))
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
async fn uses_endpoint_links_and_unlinks_projects() {
    let router = app();
    for id in ["teacher", "student"] {
        let (status, _) = call(
            router.clone(),
            "POST",
            "/v1/projects",
            Some(json!({
                "user": "adeel",
                "project_id": id,
                "name": id,
                "descriptor": format!("{id} tutoring project")
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    // a memory + parameter in `teacher`
    let (status, _) = call(
        router.clone(),
        "POST",
        "/v1/remember",
        Some(json!({
            "user": "adeel",
            "project_id": "teacher",
            "text": "prefers analogies from games"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // link student → teacher over the uses endpoint
    let (status, body) = call(
        router.clone(),
        "POST",
        "/v1/projects/uses",
        Some(json!({
            "user": "adeel",
            "project_id": "student",
            "add": "teacher"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["uses"], json!(["teacher"]));

    // the borrowed memory surfaces in student's recall (serving warm)
    let (status, body) = call(
        router.clone(),
        "POST",
        "/v1/recall",
        Some(json!({
            "user": "adeel",
            "project_id": "student",
            "query": "prefers analogies from games",
            "min_similarity": 0.05
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let hits = body["hits"].as_array().unwrap();
    assert!(
        hits.iter()
            .any(|h| h["text"].as_str().unwrap().contains("analogies")
                && h["level"] == json!("L2")
                && h["project_id"] == json!("teacher")),
        "borrowed L1 must surface as L2: {hits:?}"
    );

    // unlinking stops the sharing
    let (status, body) = call(
        router.clone(),
        "POST",
        "/v1/projects/uses",
        Some(json!({
            "user": "adeel",
            "project_id": "student",
            "remove": "teacher"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // `uses` serializes only when non-empty
    assert!(body["uses"]
        .as_array()
        .map(|a| a.is_empty())
        .unwrap_or(true));

    // exactly one of add/remove is required; unknown projects 404
    let (status, _) = call(
        router.clone(),
        "POST",
        "/v1/projects/uses",
        Some(json!({ "user": "adeel", "project_id": "student" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = call(
        router.clone(),
        "POST",
        "/v1/projects/uses",
        Some(json!({
            "user": "adeel",
            "project_id": "student",
            "add": "ghost"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn group_rename_endpoint_moves_and_merges() {
    let router = app();
    for (id, group) in [("a", "g1"), ("b", "g1"), ("c", "g2")] {
        let (status, _) = call(
            router.clone(),
            "POST",
            "/v1/projects",
            Some(json!({
                "user": "adeel",
                "project_id": id,
                "name": id,
                "group": group
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
    let (status, _) = call(
        router.clone(),
        "POST",
        "/v1/remember",
        Some(json!({
            "user": "adeel",
            "level": "L2",
            "group": "g1",
            "text": "group-wide convention"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // rename g1 → g2 (a merge: g2 already exists)
    let (status, body) = call(
        router.clone(),
        "POST",
        "/v1/projects/group/rename",
        Some(json!({ "user": "adeel", "from": "g1", "to": "g2" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["projects"], json!(2));
    assert_eq!(body["records"], json!(1));

    let (_, projects) = call(router.clone(), "GET", "/v1/projects/adeel", None).await;
    let projects = projects.as_array().unwrap();
    assert!(projects.iter().all(|p| p["group"].as_str() != Some("g1")));
    assert_eq!(
        projects
            .iter()
            .find(|p| p["project_id"] == json!("a"))
            .unwrap()["group"],
        json!("g2")
    );

    // unknown source group → 400
    let (status, _) = call(
        router.clone(),
        "POST",
        "/v1/projects/group/rename",
        Some(json!({ "user": "adeel", "from": "ghost", "to": "g2" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
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

#[tokio::test]
async fn dashboard_embeds_snapshot_and_escapes_store_content() {
    let router = app();
    call(
        router.clone(),
        "POST",
        "/v1/projects",
        Some(json!({
            "user": "adeel",
            "project_id": "teacher",
            "name": "Teacher </script>alert(1)<script>",
            "descriptor": "ai tutoring studio"
        })),
    )
    .await;
    call(
        router.clone(),
        "POST",
        "/v1/projects/group",
        Some(json!({ "user": "adeel", "project_id": "teacher", "group": "tutors" })),
    )
    .await;

    let req = Request::builder()
        .method("GET")
        .uri("/?user=adeel")
        .body(Body::empty())
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let html = String::from_utf8(bytes.to_vec()).unwrap();

    assert!(
        html.contains("window.__INITIAL__ = {"),
        "first-paint snapshot embedded"
    );
    assert!(
        html.contains("tutors"),
        "group data present in the snapshot"
    );
    assert!(
        html.contains("teacher"),
        "project data present in the snapshot"
    );
    // the name contains </script> — it must be escaped so store content can
    // never close the embedding tag (the JSON-safe <\/ form parses back
    // identically inside JS)
    assert!(
        !html.contains("</script>alert"),
        "store content must not break out of the embedding tag"
    );
    assert!(html.contains(r"<\/script>alert"));
}
