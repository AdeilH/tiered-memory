//! The HTTP service (`serve`) and shell integration (`env`).

use std::sync::Arc;

use crate::{data_root, store};
use tiered_memory::{EmbedderConfig, EngineConfig, MemoryEngine, ServerState, DEFAULT_BIND};

pub(crate) async fn serve() -> Result<(), String> {
    let root = data_root();
    let bind = std::env::var("TM_BIND").unwrap_or_else(|_| DEFAULT_BIND.into());

    let embedder = EmbedderConfig::from_env()
        .map_err(|e| e.to_string())?
        .build()
        .map_err(|e| e.to_string())?;
    let engine = Arc::new(MemoryEngine::new(
        store()?,
        embedder,
        EngineConfig::from_env(),
    ));

    // Optional bearer token: if `{data}/token` exists, its trimmed contents
    // become the required secret (all routes except /v1/health).
    let token = std::fs::read_to_string(root.join("token"))
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());
    let auth = if token.is_some() {
        "token auth ON"
    } else {
        "no token (loopback only)"
    };

    refuse_unsafe_bind(&bind, &root, token.is_some())?;

    let health = engine.health();
    let app = tiered_memory::build_router(Arc::new(ServerState {
        engine: engine.clone(),
        token,
    }));

    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .map_err(|e| format!("cannot bind {bind}: {e}"))?;
    eprintln!(
        "tiered-memory v{} | embedder {} ({} dims) | data: {} | {}",
        health.version,
        health.embedder,
        health.dims,
        root.display(),
        auth
    );
    eprintln!("endpoints: POST /v1/remember /v1/recall /v1/params /v1/feedback /v1/projects /v1/projects/group /v1/consolidate /v1/forget /v1/reindex · GET /v1/health /v1/stats/{{user}} /v1/projects/{{user}} /v1/context/{{user}}[/{{project}}] · CLI: init, projects, select, group, params, remember, recall, stats");

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            eprintln!("tiered-memory: shutting down");
        })
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Safety rail (docs/SECURITY_ANALYSIS.md M5): a non-loopback bind turns the
/// service into a network API — require a token, or an explicit opt-out.
fn refuse_unsafe_bind(bind: &str, root: &std::path::Path, has_token: bool) -> Result<(), String> {
    let host = bind.rsplit_once(':').map(|(h, _)| h).unwrap_or(bind);
    let loopback = matches!(host, "" | "127.0.0.1" | "localhost" | "::1" | "[::1]");
    if !loopback && !has_token && std::env::var("TM_ALLOW_INSECURE").as_deref() != Ok("1") {
        return Err(format!(
            "refusing to bind non-loopback address `{bind}` without auth — write a secret to {} (Bearer token) or set TM_ALLOW_INSECURE=1 to override",
            root.join("token").display()
        ));
    }
    Ok(())
}

// -- env ---------------------------------------------------------------------

/// `tiered-memory env` — emit-and-eval exports.
///
/// A process cannot modify its parent shell's environment (the env is copied
/// at fork and never propagated back), so instead of exporting, this prints
/// shell code the caller applies with `eval "$(tiered-memory env)"` — the
/// same pattern used by direnv-style tools. Prints the binary's own directory
/// for PATH (found via /proc self-exe, not $0) and the resolved data dir.
pub(crate) fn print_env() -> Result<(), String> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            println!("export PATH={}:$PATH", shell_quote(&dir.to_string_lossy()));
        }
    }
    println!(
        "export TM_DATA_DIR={}",
        shell_quote(&data_root().to_string_lossy())
    );
    Ok(())
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
