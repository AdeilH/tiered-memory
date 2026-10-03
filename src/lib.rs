//! # tiered-memory
//!
//! CPU-cache-style layered long-term memory for learner personalization.
//!
//! Like an L1/L2/L3 cache, learner memories live at three depths:
//!
//! * **L1** — tiny and hot: preferences for the project the learner is in right now.
//! * **L2** — warm: memories from related scopes (other components of the same
//!   project — frontend/backend — and similar projects), surfaced to the current one.
//! * **L3** — cold and global: traits that hold across every project.
//!
//! Reads probe L1 → L2 → L3 and promote what gets used (write-allocate); writes
//! land in L1 and overflow back into L2 (write-back); a consolidation pass merges,
//! expires, forgets, and lifts cross-project agreement into global traits.
//! The adjusted learner parameter set is derived at read time — nearest layer wins.
//!
//! ```no_run
//! use std::sync::Arc;
//! use tiered_memory::{MemoryEngine, EngineConfig, JsonFileStore, EmbedderConfig, RememberInput, RecallInput};
//!
//! # fn main() -> tiered_memory::Result<()> {
//! let store = Arc::new(JsonFileStore::new("./data")?);
//! let embedder = EmbedderConfig::default().build()?;
//! let engine = MemoryEngine::new(store, embedder, EngineConfig::default());
//!
//! engine.remember(RememberInput {
//!     user: "adeel".into(),
//!     text: "Prefers analogies from games".into(),
//!     ..Default::default()
//! })?;
//! let out = engine.recall(RecallInput {
//!     user: "adeel".into(),
//!     query: "how should I picture this?".into(),
//!     ..Default::default()
//! })?;
//! println!("{:#?}", out.hits);
//! # Ok(())
//! # }
//! ```

pub mod embed;
pub mod engine;
pub mod error;
pub mod params;
pub mod store;
pub mod types;
pub mod vector;

#[cfg(feature = "server")]
pub mod api;
// LLM-assisted memory sync (OpenAI-compatible extraction) + its client.
// Available whenever an HTTP stack is: both `sync` and the `http` embedder
// share the same OpenAI-compatible provider credentials.
#[cfg(any(feature = "server", feature = "http"))]
pub mod llm;
#[cfg(feature = "server")]
pub mod sync;
// Interactive terminal UI (credentials wizard with searchable model picker).
#[cfg(feature = "server")]
pub mod tui;
// Harness registry + skill installer (SKILL.md copies, AGENTS.md blocks).
#[cfg(feature = "server")]
pub mod harnesses;
// Read-only terminal dashboard (`tiered-memory console`).
#[cfg(feature = "server")]
pub mod console;

#[cfg(feature = "http")]
pub use embed::http::HttpEmbedder;
#[cfg(feature = "local")]
pub use embed::local::{LocalEmbedder, LocalEmbedderConfig};
pub use embed::{Embedder, EmbedderConfig};
pub use engine::{
    system_now_ms, ConsolidationReport, Counts, EngineConfig, EngineStats, FeedbackInput,
    ForgetInput, HealthInfo, MemoryContext, MemoryEngine, MemoryLine, NowFn, ProjectInput,
    RecallHit, RecallInput, RecallOutput, RememberInput, RememberOutcome, NO_GROUP,
};
pub use error::{MemoryError, Result};
pub use params::{ParamAlternative, ParamSuggestion};
pub use store::{default_data_dir, JsonFileStore, LayeredDirStore, MemoryStore, LOCAL_USER};
pub use types::{Level, MemoryKind, MemoryRecord, ParamValue, ProjectInfo, UserDb};

#[cfg(feature = "server")]
pub use api::{build_router, ServerState};
#[cfg(feature = "server")]
pub use llm::{LlmClient, LlmConfig, CREDENTIALS_FILE};
#[cfg(feature = "server")]
pub use sync::{apply, plan, sync, SyncEntry, SyncInput, SyncPlan, SyncReport};

/// Default HTTP bind address for the bundled server.
pub const DEFAULT_BIND: &str = "127.0.0.1:7900";
