//! HTTP JSON API over the engine. Language-agnostic by design — any host
//! (Node, Python, another Rust binary) drives memory over these endpoints.

use crate::engine::{
    ConsolidationReport, EngineStats, FeedbackInput, ForgetInput, HealthInfo, MemoryEngine,
    ProjectInput, RecallInput, RecallOutput, RememberInput, RememberOutcome,
};
use crate::error::MemoryError;
use crate::types::{ParamValue, ProjectInfo};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
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
        .with_state(state)
}

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
    Ok(Json(
        blocking(&state, move |e| e.register_project(req)).await?,
    ))
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
    Ok(Json(
        blocking(&state, move |e| {
            e.set_project_group(&req.user, &req.project_id, req.group.as_deref())
        })
        .await?,
    ))
}

async fn list_projects(
    State(state): State<Arc<ServerState>>,
    Path(ListProjectsPath { user }): Path<ListProjectsPath>,
) -> ApiResult<Vec<ProjectInfo>> {
    Ok(Json(
        blocking(&state, move |e| e.list_projects(&user)).await?,
    ))
}

/// Unregister a project and forget all of its records (every level).
async fn remove_project(
    State(state): State<Arc<ServerState>>,
    Path((user, project)): Path<(String, String)>,
) -> ApiResult<usize> {
    Ok(Json(
        blocking(&state, move |e| e.remove_project(&user, &project)).await?,
    ))
}

async fn remember(
    State(state): State<Arc<ServerState>>,
    Json(req): Json<RememberInput>,
) -> ApiResult<RememberOutcome> {
    Ok(Json(blocking(&state, move |e| e.remember(req)).await?))
}

async fn recall(
    State(state): State<Arc<ServerState>>,
    Json(req): Json<RecallInput>,
) -> ApiResult<RecallOutput> {
    Ok(Json(blocking(&state, move |e| e.recall(req)).await?))
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
    Ok(Json(blocking(&state, move |e| e.feedback(req)).await?))
}

#[derive(Deserialize)]
struct ConsolidateBody {
    user: String,
}

async fn consolidate(
    State(state): State<Arc<ServerState>>,
    Json(req): Json<ConsolidateBody>,
) -> ApiResult<ConsolidationReport> {
    Ok(Json(
        blocking(&state, move |e| e.consolidate(&req.user)).await?,
    ))
}

async fn forget(
    State(state): State<Arc<ServerState>>,
    Json(req): Json<ForgetInput>,
) -> ApiResult<usize> {
    Ok(Json(blocking(&state, move |e| e.forget(req)).await?))
}

#[derive(Deserialize)]
struct ReindexBody {
    user: String,
}

async fn reindex(
    State(state): State<Arc<ServerState>>,
    Json(req): Json<ReindexBody>,
) -> ApiResult<usize> {
    Ok(Json(blocking(&state, move |e| e.reindex(&req.user)).await?))
}
