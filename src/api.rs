//! HTTP JSON API over the engine. Language-agnostic by design — any host
//! (Node, Python, another Rust binary) drives memory over these endpoints.

use crate::engine::{
    ConsolidationReport, EngineStats, FeedbackInput, ForgetInput, HealthInfo, MemoryEngine,
    ProjectInput, RecallInput, RecallOutput, RememberInput, RememberOutcome,
};
use crate::error::MemoryError;
use crate::types::{ParamValue, ProjectInfo};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;

pub struct ServerState {
    pub engine: Arc<MemoryEngine>,
    /// When set, every route except `/v1/health` requires
    /// `Authorization: Bearer <token>`. Opt-in for shared machines; the
    /// standalone default is loopback-only with no token.
    pub token: Option<String>,
}

pub fn build_router(state: Arc<ServerState>) -> Router {
    // Everything registered before `route_layer` sits behind the auth check;
    // health stays open so monitoring works without a token.
    Router::new()
        .route("/v1/stats/{user}", get(stats))
        .route("/v1/context/{user}", get(context_no_project))
        .route("/v1/context/{user}/{project}", get(context))
        .route("/v1/projects", post(projects))
        .route("/v1/projects/group", post(set_group))
        .route("/v1/projects/group/rename", post(rename_group))
        .route("/v1/projects/uses", post(set_uses))
        .route("/v1/projects/{user}", get(list_projects))
        .route("/v1/projects/{user}/{project}", delete(remove_project))
        .route("/v1/remember", post(remember))
        .route("/v1/recall", post(recall))
        .route("/v1/params", post(params))
        .route("/v1/feedback", post(feedback))
        .route("/v1/consolidate", post(consolidate))
        .route("/v1/forget", post(forget))
        .route("/v1/reindex", post(reindex))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_token,
        ))
        .route("/v1/health", get(health))
        .route("/", get(dashboard))
        .layer(axum::middleware::from_fn(log_requests))
        .with_state(state)
}

/// One line per request — method, path, status, duration. Health probes and
/// dashboard poll GETs are skipped (reads are opt-in via TM_VERBOSE=1);
/// operations and errors always log. `TM_QUIET=1` silences everything.
async fn log_requests(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let method = req.method().to_string();
    let path = req.uri().path().to_string();
    let start = std::time::Instant::now();
    let response = next.run(req).await;
    let verbose = std::env::var("TM_VERBOSE").as_deref() == Ok("1");
    let skip = path == "/v1/health" || (method == "GET" && !verbose);
    if !skip && logging_enabled() {
        println!(
            "tm: {method} {path} → {} ({})",
            response.status().as_u16(),
            fmt_duration(start.elapsed())
        );
    }
    response
}

fn logging_enabled() -> bool {
    std::env::var("TM_QUIET").as_deref() != Ok("1")
}

fn fmt_duration(d: std::time::Duration) -> String {
    let ms = d.as_secs_f64() * 1000.0;
    if ms >= 10.0 {
        format!("{ms:.0}ms")
    } else {
        format!("{ms:.1}ms")
    }
}

/// One line per meaningful operation (writes, deletions, maintenance).
fn log_op(msg: String) {
    if logging_enabled() {
        println!("tm: {msg}");
    }
}

/// Browser dashboard at the service root — a live, self-contained page
/// (assets/dashboard.html): first paint from a server-injected snapshot,
/// then polls /v1/stats + /v1/projects every 3s. Read-only;
/// `?user=<name>` selects a non-default user.
async fn dashboard(
    State(state): State<Arc<ServerState>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Html<String> {
    let user = params
        .get("user")
        .cloned()
        .unwrap_or_else(|| crate::LOCAL_USER.to_string());
    let stats = blocking(&state, {
        let u = user.clone();
        move |e| e.stats(&u)
    })
    .await;
    let projects = blocking(&state, {
        let u = user.clone();
        move |e| e.list_projects(&u)
    })
    .await;
    let payload = match (stats, projects) {
        (Ok(s), Ok(p)) => serde_json::json!({ "user": user, "stats": s, "projects": p }),
        (Err(e), _) | (_, Err(e)) => serde_json::json!({
            "user": user, "stats": null, "projects": null, "error": e.message
        }),
    };
    // embedded for the page's first paint; `</` is escaped so store content
    // can never close the script tag — the client renders with textContent
    let mut json = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".into());
    json = json.replace("</", "<\\/");
    Html(DASHBOARD.replace("__SNAPSHOT__", &json))
}

/// The dashboard page (HTML + CSS + JS, no external assets, works offline).
const DASHBOARD: &str = include_str!("../assets/dashboard.html");

async fn require_token(
    State(state): State<Arc<ServerState>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    match &state.token {
        None => next.run(req).await,
        Some(expected) => {
            let supplied = req
                .headers()
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok());
            if supplied == Some(&format!("Bearer {expected}")) {
                next.run(req).await
            } else {
                (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({ "error": "missing or invalid bearer token" })),
                )
                    .into_response()
            }
        }
    }
}

// -- error mapping -----------------------------------------------------------

pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl From<MemoryError> for ApiError {
    fn from(e: MemoryError) -> Self {
        let status = match &e {
            MemoryError::EmbedderMismatch { .. } => StatusCode::CONFLICT,
            MemoryError::UserNotFound(_)
            | MemoryError::MemoryNotFound(_)
            | MemoryError::ProjectNotFound(_) => StatusCode::NOT_FOUND,
            MemoryError::Invalid(_) => StatusCode::BAD_REQUEST,
            MemoryError::Embedder(_)
            | MemoryError::Storage(_)
            | MemoryError::Serialization(_)
            | MemoryError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        ApiError {
            status,
            message: e.to_string(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        log_op(format!("error {} {}", self.status.as_u16(), self.message));
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

type ApiResult<T> = std::result::Result<Json<T>, ApiError>;

/// Engine calls are synchronous (embedding included); push them off the
/// async runtime so one slow provider never blocks the event loop.
async fn blocking<T, F>(state: &Arc<ServerState>, f: F) -> Result<T, ApiError>
where
    F: FnOnce(&MemoryEngine) -> crate::error::Result<T> + Send + 'static,
    T: Send + 'static,
{
    let engine = state.engine.clone();
    tokio::task::spawn_blocking(move || f(&engine))
        .await
        .map_err(|e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("worker panicked: {e}"),
        })?
        .map_err(ApiError::from)
}

// -- handlers ----------------------------------------------------------------

async fn health(State(state): State<Arc<ServerState>>) -> ApiResult<HealthInfo> {
    Ok(Json(blocking(&state, |e| Ok(e.health())).await?))
}

async fn stats(
    State(state): State<Arc<ServerState>>,
    Path(user): Path<String>,
) -> ApiResult<EngineStats> {
    Ok(Json(blocking(&state, move |e| e.stats(&user)).await?))
}

/// The gathered L1/L2/L3 state + adjusted parameters for one project scope —
/// the same view `sync` gathers and the session-start hook renders as the
/// learner brief. Without a project, only L3 is visible.
async fn context_no_project(
    State(state): State<Arc<ServerState>>,
    Path(user): Path<String>,
) -> ApiResult<crate::engine::MemoryContext> {
    Ok(Json(
        blocking(&state, move |e| e.memory_context(&user, None, 12)).await?,
    ))
}

async fn context(
    State(state): State<Arc<ServerState>>,
    Path((user, project)): Path<(String, String)>,
) -> ApiResult<crate::engine::MemoryContext> {
    Ok(Json(
        blocking(&state, move |e| e.memory_context(&user, Some(&project), 12)).await?,
    ))
}

async fn projects(
    State(state): State<Arc<ServerState>>,
    Json(req): Json<ProjectInput>,
) -> ApiResult<ProjectInfo> {
    let user = req.user.clone();
    let id = req.project_id.clone();
    let info = blocking(&state, move |e| e.register_project(req)).await?;
    log_op(format!(
        "registered project `{id}` for `{user}` (similar: {})",
        info.similar.join(", ")
    ));
    Ok(Json(info))
}

#[derive(Deserialize)]
struct ListProjectsPath {
    user: String,
}

/// Assign the project's L2 group: `{"group": "rust-clis"}` to assign,
/// `{"group": "none"}` to record an explicit no-group confirmation,
/// `{"group": null}` to reset to unassigned.
#[derive(Deserialize)]
struct GroupBody {
    user: String,
    project_id: String,
    #[serde(default)]
    group: Option<String>,
}

async fn set_group(
    State(state): State<Arc<ServerState>>,
    Json(req): Json<GroupBody>,
) -> ApiResult<ProjectInfo> {
    let (user, project, group) = (req.user.clone(), req.project_id.clone(), req.group.clone());
    let info = blocking(&state, move |e| {
        e.set_project_group(&req.user, &req.project_id, req.group.as_deref())
    })
    .await?;
    log_op(format!(
        "group of `{project}` ({user}) → {}",
        group.as_deref().unwrap_or("(unassigned)")
    ));
    Ok(Json(info))
}

async fn list_projects(
    State(state): State<Arc<ServerState>>,
    Path(ListProjectsPath { user }): Path<ListProjectsPath>,
) -> ApiResult<Vec<ProjectInfo>> {
    Ok(Json(
        blocking(&state, move |e| e.list_projects(&user)).await?,
    ))
}

/// Rename an L2 group everywhere (projects + group-owned memories); renaming
/// onto an existing group merges the two. → `{"projects": n, "records": m}`.
#[derive(Deserialize)]
struct GroupRenameBody {
    user: String,
    from: String,
    to: String,
}

async fn rename_group(
    State(state): State<Arc<ServerState>>,
    Json(req): Json<GroupRenameBody>,
) -> ApiResult<serde_json::Value> {
    let (user, from, to) = (req.user.clone(), req.from.clone(), req.to.clone());
    let (f2, t2) = (from.clone(), to.clone());
    let (projects, records) = blocking(&state, move |e| e.rename_group(&user, &from, &to)).await?;
    log_op(format!(
        "group `{f2}` → `{t2}` for `{}`: {projects} project(s), {records} group-owned memory/memories moved",
        req.user
    ));
    Ok(Json(json!({ "projects": projects, "records": records })))
}

/// Add or remove a cross-project memory source for one project. Exactly one
/// of `add` / `remove`, naming the *other* project: `{"add": "a"}` makes the
/// project see a's L1+L2 memories (from its warm L2 tier — directional, a
/// gains nothing); `{"remove": "a"}` drops the link again.
#[derive(Deserialize)]
struct UsesBody {
    user: String,
    project_id: String,
    #[serde(default)]
    add: Option<String>,
    #[serde(default)]
    remove: Option<String>,
}

async fn set_uses(
    State(state): State<Arc<ServerState>>,
    Json(req): Json<UsesBody>,
) -> ApiResult<ProjectInfo> {
    let (adding, target) = match (req.add.as_deref(), req.remove.as_deref()) {
        (Some(t), None) => (true, t),
        (None, Some(t)) => (false, t),
        _ => {
            return Err(ApiError {
                status: StatusCode::BAD_REQUEST,
                message: "needs exactly one of `add` or `remove`".into(),
            })
        }
    };
    let (user, project, target) = (req.user.clone(), req.project_id.clone(), target.to_string());
    let (u2, p2, t2) = (user.clone(), project.clone(), target.clone());
    let info = blocking(&state, move |e| {
        if adding {
            e.add_project_use(&user, &project, &target)
        } else {
            e.remove_project_use(&user, &project, &target)
        }
    })
    .await?;
    log_op(format!(
        "uses of `{p2}` ({u2}): {} `{t2}` → {:?}",
        if adding { "+" } else { "-" },
        info.uses
    ));
    Ok(Json(info))
}

/// Unregister a project and forget all of its records (every level).
async fn remove_project(
    State(state): State<Arc<ServerState>>,
    Path((user, project)): Path<(String, String)>,
) -> ApiResult<usize> {
    let (u2, p2) = (user.clone(), project.clone());
    let removed = blocking(&state, move |e| e.remove_project(&u2, &p2)).await?;
    log_op(format!(
        "removed project `{project}` ({user}) — {removed} memories forgotten"
    ));
    Ok(Json(removed))
}

async fn remember(
    State(state): State<Arc<ServerState>>,
    Json(req): Json<RememberInput>,
) -> ApiResult<RememberOutcome> {
    let (user, project, level) = (req.user.clone(), req.project_id.clone(), req.level);
    let out = blocking(&state, move |e| e.remember(req)).await?;
    log_op(format!(
        "remember `{}` user={user} project={} level={} deduped={} demoted_to_l2={}",
        out.id,
        project.as_deref().unwrap_or("-"),
        level
            .map(|l| format!("{l:?}"))
            .unwrap_or_else(|| "auto".into()),
        out.deduped,
        out.demoted_to_l2
    ));
    if out.auto_consolidated {
        log_op("auto-consolidation ran (every N writes)".into());
    }
    Ok(Json(out))
}

async fn recall(
    State(state): State<Arc<ServerState>>,
    Json(req): Json<RecallInput>,
) -> ApiResult<RecallOutput> {
    let (user, project, query) = (req.user.clone(), req.project_id.clone(), req.query.clone());
    let out = blocking(&state, move |e| e.recall(req)).await?;
    log_op(format!(
        "recall \"{query}\" user={user} project={} → {} hit(s), {} promoted",
        project.as_deref().unwrap_or("-"),
        out.hits.len(),
        out.promoted.len()
    ));
    Ok(Json(out))
}

#[derive(Debug, Deserialize)]
pub struct ParamsRequest {
    pub user: String,
    #[serde(default)]
    pub project_id: Option<String>,
    /// The host app's baseline values; learner adjustments override these.
    #[serde(default)]
    pub defaults: Option<BTreeMap<String, ParamValue>>,
}

#[derive(Debug, Serialize)]
pub struct ParamSuggestionDto {
    pub key: String,
    pub value: ParamValue,
    pub source: crate::types::Level,
    pub confidence: f32,
    pub updated_at_ms: u64,
    pub alternatives: Vec<crate::params::ParamAlternative>,
}

#[derive(Debug, Serialize)]
pub struct ParamsResponse {
    /// Ready-to-apply map: defaults merged under learner adjustments.
    pub params: BTreeMap<String, ParamValue>,
    /// Full detail per key (source layer, confidence, conflicts).
    pub detail: BTreeMap<String, ParamSuggestionDto>,
}

async fn params(
    State(state): State<Arc<ServerState>>,
    Json(req): Json<ParamsRequest>,
) -> ApiResult<ParamsResponse> {
    let defaults = req.defaults.unwrap_or_default();
    let project_id = req.project_id.clone();
    blocking(&state, move |e| {
        let suggestions = e.adjusted_parameters(&req.user, project_id.as_deref())?;
        let mut detail = BTreeMap::new();
        let mut merged = defaults.clone();
        for s in suggestions {
            merged.insert(s.key.clone(), s.value.clone());
            detail.insert(
                s.key.clone(),
                ParamSuggestionDto {
                    key: s.key,
                    value: s.value,
                    source: s.source,
                    confidence: s.confidence,
                    updated_at_ms: s.updated_at_ms,
                    alternatives: s.alternatives,
                },
            );
        }
        Ok(ParamsResponse {
            params: merged,
            detail,
        })
    })
    .await
    .map(Json)
}

async fn feedback(
    State(state): State<Arc<ServerState>>,
    Json(req): Json<FeedbackInput>,
) -> ApiResult<RememberOutcome> {
    let (user, key, global) = (
        req.user.clone(),
        req.key.clone(),
        req.global.unwrap_or(false),
    );
    let out = blocking(&state, move |e| e.feedback(req)).await?;
    log_op(format!(
        "feedback {key} user={user} scope={} deduped={}",
        if global { "global" } else { "project" },
        out.deduped
    ));
    Ok(Json(out))
}

#[derive(Deserialize)]
struct ConsolidateBody {
    user: String,
}

async fn consolidate(
    State(state): State<Arc<ServerState>>,
    Json(req): Json<ConsolidateBody>,
) -> ApiResult<ConsolidationReport> {
    let user = req.user.clone();
    let report = blocking(&state, move |e| e.consolidate(&req.user)).await?;
    log_op(format!(
        "consolidate user={user}: {} expired, {} forgotten, {} merged, {} traits lifted",
        report.expired, report.forgotten, report.merged, report.traits_lifted
    ));
    Ok(Json(report))
}

async fn forget(
    State(state): State<Arc<ServerState>>,
    Json(req): Json<ForgetInput>,
) -> ApiResult<usize> {
    let (user, id, project, level, all) = (
        req.user.clone(),
        req.id.clone(),
        req.project_id.clone(),
        req.level,
        req.all,
    );
    let removed = blocking(&state, move |e| e.forget(req)).await?;
    let scope = if all == Some(true) {
        "everything".to_string()
    } else if let Some(id) = &id {
        format!("id {id}")
    } else if let Some(p) = &project {
        format!("project {p}")
    } else if let Some(l) = level {
        format!("level {l:?}")
    } else {
        "?".to_string()
    };
    log_op(format!("forgot {removed} memories ({scope}) user={user}"));
    Ok(Json(removed))
}

#[derive(Deserialize)]
struct ReindexBody {
    user: String,
}

async fn reindex(
    State(state): State<Arc<ServerState>>,
    Json(req): Json<ReindexBody>,
) -> ApiResult<usize> {
    let user = req.user.clone();
    let n = blocking(&state, move |e| e.reindex(&req.user)).await?;
    log_op(format!("reindexed {n} memories user={user}"));
    Ok(Json(n))
}
