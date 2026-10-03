# tiered-memory

A **layered long-term memory engine for learner personalization**, built like a CPU
cache hierarchy — in Rust, with an **embedded local LLM for vectorization** and a
pluggable embedder interface so any provider can be swapped in.

It answers two questions for any host application (tutor, coach, coding agent, ...):

1. *What should I remember about this learner right now?* → **recall**
2. *What parameter values should I run with for this learner in this project?* → **params**

```
                 recall("how do I explain recursion?")
                        │
        ┌───────────────▼────────────────┐
        │  L1  hot · project-local       │  preferences for the project the
        │      (this project, ~128 lines)│  learner is in right now
        ├────────────────────────────────┤
        │  L2  warm · related scopes     │  other components of the same project
        │      (similar/sibling scopes)  │  (frontend ↔ backend) + similar projects
        ├────────────────────────────────┤
        │  L3  cold · global             │  traits that hold across ALL projects
        │      (~4096 lines)             │
        └────────────────────────────────┘
```

Exactly like cache: **L1 is smallest and fastest to serve, L3 is biggest and
global.** Reads probe L1 → L2 → L3, hot memories get promoted up, evicted L1
lines are written back into L2, and a consolidation pass merges duplicates,
expires stale lines, and lifts cross-project agreement into L3 traits.

## Why not just one big vector store?

A flat store forces one context. A learner's *current project* preferences
("in this course I want pure theory, no code") should override their *global*
profile ("I like games analogies"), while similar work ("my other React app")
should contribute without polluting. Layering gives you that precedence for free:

| | L1 | L2 | L3 |
|---|---|---|---|
| Scope | one project | related scopes | every project |
| Typical content | project prefs, feedback events | sibling-component + similar-project knowledge | learner traits, lifted patterns |
| Capacity (default) | 128 | 1024 | 4096 |
| Rank weight in recall | 1.0 | 0.9 | 0.85 |

When the same parameter key exists at several layers, **the nearest layer wins**
(L1 > L2 > L3) and deeper conflicting values are returned as `alternatives` —
cache coherence for preferences.

## Features

- **Cache semantics**: write-through/upsert, write-back eviction (demoted L1
  lines land in L2, nothing is silently lost), write-allocate promotion
  (memories recalled repeatedly from L2/L3 are copied into L1), pinning, TTLs.
- **Learner parameters**: memories carry structured `key → value` assertions
  (`difficulty: 0.35`, `analogy_domain: "games"`). The adjusted parameter set is
  derived at read time — nearest layer wins, freshest+most-confident wins inside
  a layer, defaults merge underneath.
- **Cross-project trait lifting**: a parameter that keeps agreeing across ≥ 2
  projects is promoted into a global L3 trait automatically, so every future
  project inherits it. Disagreeing projects are left alone.
- **Similar-project federation (L2)**: projects register a descriptor; projects
  whose descriptors embed close share L2 memories. Same-project components
  (frontend/backend) always share.
- **Swappable vectorizer** — the `Embedder` trait:
  - `hashing` (default): dependency-free, deterministic, offline. Lexical
    similarity only — good for tests/CI.
  - `local` (flag): **embedded LLM vectorization** — a real sentence-transformer
    (MiniLM-L6, 384 dims) running in-process on CPU via
    [candle](https://github.com/huggingface/candle). No Python, no server.
  - `http` (flag): any OpenAI-compatible `/embeddings` endpoint (OpenAI,
    Ollama, LM Studio, vLLM, ...).
  - Switching backends is safe: stores carry an embedder fingerprint and refuse
    to mix geometries; `POST /v1/reindex` re-embeds everything.
- **Two ways to use it**: as a Rust library (`MemoryEngine`) or as a standalone
  HTTP service (`tiered-memory-server`) any language can drive.

## Quickstart (Rust library)

```bash
cargo run --example basic
```

```rust
use std::sync::Arc;
use tiered_memory::{MemoryEngine, EngineConfig, JsonFileStore, EmbedderConfig};

let store = Arc::new(JsonFileStore::new("./data")?);
let embedder = EmbedderConfig::default().build()?; // hashing, offline
let engine = MemoryEngine::new(store, embedder, EngineConfig::default());

// register the learner's project scope
engine.register_project(ProjectInput {
    user: "adeel".into(),
    project_id: "teacher".into(),
    components: vec!["frontend".into(), "backend".into()],
    ..Default::default()
})?;

// capture a preference
engine.feedback(FeedbackInput {
    user: "adeel".into(),
    key: "difficulty".into(),
    value: ParamValue::Number(0.35),
    project_id: Some("teacher".into()),
    ..Default::default()
})?;

// get adjusted parameters (defaults merged under learner adjustments)
let params = engine.parameters_with_defaults("adeel", Some("teacher"), &defaults)?;
```

## Quickstart (HTTP service)

```bash
# offline default (hashing embedder):
TM_DATA_DIR=./data cargo run --release --bin tiered-memory -- serve

# embedded local LLM vectorization (downloads MiniLM once, ~90 MB, then cached):
TM_DATA_DIR=./data cargo run --release --features local --bin tiered-memory -- serve
```

Then:

```bash
curl -s localhost:7900/v1/health

curl -s -X POST localhost:7900/v1/feedback -H 'content-type: application/json' -d '{
  "user": "local", "project_id": "teacher",
  "key": "analogy_domain", "value": "games", "weight": 0.9 }'

curl -s -X POST localhost:7900/v1/params -H 'content-type: application/json' -d '{
  "user": "local", "project_id": "teacher",
  "defaults": { "difficulty": 0.5, "lesson_style": "standard" } }'
```

## Install

The binary installs with cargo and is then usable from **any** project — Rust
or not. Projects do *not* take it as a dependency; they talk to the installed
binary over HTTP or by shelling out to the CLI.

```bash
# from a checkout / submodule
cargo install --path tiered-memory          # add --features local for the embedded model

# straight from git
cargo install --git https://github.com/<you>/tiered-memory
```

This puts one `tiered-memory` binary on your PATH (default `~/.cargo/bin`) —
it is the HTTP service *and* the CLI.

## The standalone binary & on-disk layout

One binary, `tiered-memory`, is both the service and a local CLI. Data lives at
`TM_DATA_DIR` (default `~/tiered-memory`) and **mirrors the cache hierarchy**:

```text
~/tiered-memory/
├── current-project                    CLI selection marker (project id)
├── meta.json                          store version + embedder fingerprint
├── projects/<project-id>.json         project descriptors (drive L2 links)
├── cache/
│   ├── L1/<project-id>/               hot — only THIS project's memories
│   │   ├── memories.json              machine record (ids, vectors — authoritative)
│   │   └── memories.md                human-readable mirror, regenerated on write
│   ├── L2/                            warm — related projects & components
│   │   ├── memories.json|md
│   │   └── similar-projects.txt       project links; HAND EDITS ARE PRESERVED
│   └── L3/                            cold — user-level traits, all projects
│       └── memories.json|md
└── users/<other-user>/…               additional users (multi-user/server mode)
```

The default user is `local` (its data sits at the root, exactly as above).
The `.md` files are mirrors — read, grep, diff them; edit through the API or
CLI so ids/vectors stay consistent. `similar-projects.txt` is the one
authoritative human file: add a line `projectA projectB` and the link survives
every automatic recomputation (it activates fully once both projects register).

### CLI

```bash
cd any-project/            # Rust, Node, Python — anything
tiered-memory init         # registers THIS project (auto-detected name/description)
                           # and writes ./tiered-memory.json so hosts/agents
                           # know the project id, user and service URL

tiered-memory projects     # every project currently using tiered memory
tiered-memory select       # numbered picker → writes current-project
tiered-memory remember "prefers worked examples" --param depth=0.7
tiered-memory recall "how should I introduce recursion?"
tiered-memory params       # adjusted parameters for this project
tiered-memory stats        # per-layer counts vs capacity
tiered-memory serve        # the HTTP service (default command)
```

Inside an init'ed project, `remember` / `recall` / `params` resolve the
project from the local `tiered-memory.json` marker — no flags needed.

Write commands (`init`, `remember`, `sync`) go **through the running service
when one is up** and fall back to writing the local store directly when it
isn't, so the CLI and a live `serve` never disagree.

### Using it from a non-Rust project

Two ways, both dependency-free:

1. **HTTP** — run `tiered-memory serve`, then use any HTTP client. A
   zero-dependency Node 18+ client ships in
   [`clients/node/tiered-memory.mjs`](clients/node/tiered-memory.mjs).
2. **Shell out** — `tiered-memory remember "…"` / `tiered-memory recall "…"`
   from scripts, npm scripts, Makefiles, agent hooks.

`tiered-memory init` in the project gives both modes the project id; the
`tiered-memory.json` it writes is the discovery file for host applications.

## The `/tiered-memory` agent skill

The repo ships a harness-ready skill package (`skill/SKILL.md`) that teaches
any agent runtime to run the full update pass: **gather** the current state of
all three layers, **extract** new or changed knowledge from the session with
one LLM call, and **write it back** into the right layers.

```bash
tiered-memory install-skill          # → ~/.agents/skills/tiered-memory/SKILL.md
tiered-memory install-skill --dir /path/to/skills   # any other harness dir
```

Then invoke it as `/tiered-memory` (or "update my memory") from the harness.
Under the hood it runs:

```bash
tiered-memory sync --stdin <<'EOF'
<the session transcript>
EOF
# gathers L1+L2+L3 → one OpenAI-compatible LLM call extracts updates →
# writes each into L1 (project), L2 (related scopes) or L3 (global traits)
# and prints what landed, per layer, plus the adjusted parameters after
```

Use `--dry-run` to preview without writing, `--file <path>` / quoted text for
input, and `--credentials <file>` to point at an alternative credentials file.
The skill is embedded in the binary at compile time, so `install-skill` always
installs the version matching the installed release — re-run it after
upgrading.

### LLM credentials (OpenAI-compatible)

`sync` talks to any OpenAI-compatible `/chat/completions` endpoint — OpenAI,
OpenRouter, Groq, Ollama (`http://localhost:11434/v1`), LM Studio, vLLM:

```bash
tiered-memory credentials set
```

runs an **interactive terminal setup**: pick the provider from a searchable
list (or type a custom base URL), enter the API key (masked, editable), then
the wizard **fetches the provider's live model catalog** from its `/models`
endpoint and lets you type-to-search it — arrow keys to move, Enter to select,
or "✎ Type a model id manually…" when a provider has no list endpoint.

For scripts, the same command takes flags and skips the TUI:

```bash
tiered-memory credentials set --base-url https://api.openai.com/v1 \
    --api-key sk-... --model gpt-4o-mini
tiered-memory models              # list the configured provider's catalog
tiered-memory credentials show    # masked
tiered-memory credentials clear
```

Credentials persist in `{data}/credentials.json` (written `0600`); the
`TM_LLM_BASE_URL` / `TM_LLM_API_KEY` / `TM_LLM_MODEL` env vars also work.
Without credentials the skill still works — the SKILL.md instructs the agent
to fall back to routing memories itself via `remember --global` / `--level`.

### Auth (opt-in, sized for a standalone binary)

The service binds to `127.0.0.1` and is open by default — for a personal
machine that's the threat model. On a shared machine, drop a secret in
`$TM_DATA_DIR/token` (e.g. `head -c 32 /dev/urandom | base64 > ~/tiered-memory/token`)
and every route except `/v1/health` requires `Authorization: Bearer <token>`.

## Use it as a Rust crate (optional)

Rust projects that want in-process access can depend on the library — same
crate as the binary:

```toml
# from a local checkout / submodule
tiered-memory = { path = "../tiered-memory" }
# or from git
tiered-memory = { git = "https://github.com/<you>/tiered-memory" }
```

```rust
use std::sync::Arc;
use tiered_memory::{MemoryEngine, EngineConfig, LayeredDirStore, EmbedderConfig};

let store = Arc::new(LayeredDirStore::new(tiered_memory::default_data_dir())?);
let engine = MemoryEngine::new(store, EmbedderConfig::default().build()?, EngineConfig::default());
```

Swap the vectorizer without touching call sites via `EmbedderConfig`
(`Hashing` / `Local` / `Http`) — see the embedder table below. Non-Rust
projects skip all of this: install the binary, run `init`, use HTTP or the CLI.

## Embedder backends

| Backend | Feature | Quality | Needs |
|---|---|---|---|
| `hashing` | always on | lexical overlap only | nothing (offline, deterministic) |
| `local` (MiniLM / bge-small) | `--features local` | real semantic embeddings | ~90 MB model download once, CPU inference in-process |
| `http` | `--features http` | depends on provider | an OpenAI-compatible `/embeddings` URL |

Model presets for `local` (`TM_MODEL`): `minilm` (default,
`sentence-transformers/all-MiniLM-L6-v2`, mean pooling) and `bge-small`
(`BAAI/bge-small-en-v1.5`, CLS pooling). Or point `TM_MODEL_DIR` at any local
directory with `config.json` + `tokenizer.json` + `model.safetensors`.

**Switching embedders**: stores record the producing embedder's fingerprint
(`name:dims`). A mismatching engine refuses to search instead of returning
garbage — call `POST /v1/reindex {"user": ...}` to re-embed in place.

## Configuration (env)

| Variable | Default | Meaning |
|---|---|---|
| `TM_DATA_DIR` | `~/tiered-memory` | data root (layered `cache/L1\|L2\|L3` layout) |
| `TM_STORE` | `layered` | `layered` (cache dir layout) or `flat` (one JSON per user) |
| `TM_USER` | `local` | default user for CLI commands |
| `TM_BIND` | `127.0.0.1:7900` | HTTP bind address |
| `TM_EMBEDDER` | `hashing` | `hashing` \| `local` \| `http` |
| `TM_EMBEDDER_DIMS` | `512` | dims for the hashing embedder |
| `TM_MODEL` | `minilm` | local preset: `minilm` \| `bge-small` |
| `TM_MODEL_DIR` | – | load a local model directory instead of the hub |
| `TM_HF_CACHE` | `{data}/hf` | where hub downloads are cached |
| `TM_HTTP_URL` / `TM_HTTP_API_KEY` / `TM_HTTP_MODEL` / `TM_HTTP_DIMS` | – | http backend settings |
| `TM_L1_CAPACITY` / `TM_L2_CAPACITY` / `TM_L3_CAPACITY` | `128/1024/4096` | layer sizes |
| `TM_CONSOLIDATE_EVERY` | `50` | auto-consolidate every N writes (0 = manual only) |
| `TM_WRITE_ALLOCATE` | `1` | promote hot deep memories into L1 on recall |
| `TM_MIN_SIMILARITY` | per-embedder | recall floor (hashing 0.15, local 0.30, http 0.25) |

## HTTP API

| Endpoint | Purpose |
|---|---|
| `GET /v1/health` | version, embedder fingerprint, user count |
| `GET /v1/stats/{user}` | per-layer counts vs capacity, projects, parameter keys |
| `POST /v1/projects` | register/update a project descriptor (drives L2 linking) |
| `GET /v1/projects/{user}` | list projects + their `similar` links |
| `POST /v1/remember` | store a memory (text, params, kind, scope, TTL, pin) |
| `POST /v1/recall` | layered semantic search → hits annotated with source layer |
| `POST /v1/params` | adjusted parameter set (+ defaults merged, + per-key detail) |
| `POST /v1/feedback` | shortcut: assert one parameter (`key`, `value`, `weight`) |
| `POST /v1/consolidate` | run the maintenance pass (expire, forget, merge, lift) |
| `POST /v1/forget` | delete by id / project / layer / all |
| `POST /v1/reindex` | re-embed everything with the current embedder |

Errors are `{"error": "..."}` with meaningful status codes (400 invalid, 404
unknown id, **409 embedder mismatch**). See [docs/API.md](docs/API.md) for
request/response shapes with examples.

## Project layout

```
src/
├── types.rs      data model: Level (L1/L2/L3), MemoryRecord, ParamValue, UserDb
├── engine.rs     the cache: write path, layered recall, promotion, consolidation
├── params.rs     parameter merging (nearest layer wins, recency × confidence)
├── embed/
│   ├── mod.rs    the Embedder trait + config/factory (the swappable interface)
│   ├── hashing.rs  dependency-free deterministic backend
│   ├── local.rs    embedded candle sentence-transformer (feature `local`)
│   └── http.rs     OpenAI-compatible provider (feature `http`)
├── store.rs      MemoryStore trait: LayeredDirStore (cache/L1|L2|L3 + MD mirrors)
│                 and JsonFileStore (flat) — both with atomic writes
├── vector.rs     cosine/normalize/hash helpers
├── api.rs        axum HTTP layer + optional bearer-token auth (feature `server`)
└── bin/cli.rs    the `tiered-memory` binary: serve · projects · select · params · stats

docs/ARCHITECTURE.md   design deep-dive: policies, invariants, trade-offs
docs/API.md            endpoint reference with request/response examples
docs/INTEGRATION.md    wiring it into an app (Node.js client included)
clients/node/tiered-memory.mjs   zero-dependency HTTP client for Node 18+
```

## Tests

```bash
cargo test                                    # 26 tests, offline, no model needed
cargo test --features local -- --ignored      # + real-model smoke test (downloads MiniLM once)
```

Engine tests cover layer precedence, eviction write-back, promotion by heat,
key-based feedback upsert, TTL expiry, similar-project L2 federation, trait
lifting (agreement lifts, conflicts don't), embedder-mismatch refusal, pinning,
and the full HTTP surface.

## Design decisions & trade-offs

- **Parameters are derived at read time**, not stored as mutable state. No
  drift bugs, no write amplification; conflicts stay visible as `alternatives`.
- **Brute-force cosine search.** A personal memory store is thousands of lines,
  not millions — linear scan over normalized vectors is sub-millisecond and
  removes an entire index subsystem. Swap in HNSW when it stops being.
- **Layered directories, JSON-authoritative + MD mirrors.** The disk looks like
  the cache (`cache/L1/<project>/`, `cache/L2/`, `cache/L3/`) so a human can
  read and grep their memory. Ids/vectors live in JSON (not human-editable);
  `memories.md` is regenerated on every write; `similar-projects.txt` is the
  one authoritative human file (hand-added links survive recomputation).
  Both stores implement the `MemoryStore` trait — SQLite/redb drop in later.
- **The embedder is the only thing that touches model weights**, so the core
  builds in seconds with zero ML dependencies by default.
- Per-user locking (engine caches loaded users); embedding happens outside
  locks. One process per data dir is the supported deployment shape.

## Roadmap ideas

- `MemoryStore` impls: SQLite (rusqlite) / redb
- NAPI-RS native Node bindings for in-process use without HTTP
- Decay curves learned from recall feedback; per-project L2 similarity clusters
  with multi-membership
- Multi-user auth layer for the HTTP service

## License

MIT
