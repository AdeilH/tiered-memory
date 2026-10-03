# tiered-memory

Layered long-term memory for AI agents and tutors — **L1/L2/L3 cache-style**,
in Rust, as a single standalone binary. It remembers what a learner prefers,
per project and globally, and hands you the adjusted parameter set at session
start.

```
recall("how do I explain recursion?")
        │ probes in order, nearest layer wins
┌───────▼──────────────────────────┐
│ L1 · hot · THIS project          │  prefs for the project you're in
├──────────────────────────────────┤
│ L2 · warm · related scopes       │  sibling components + similar projects
├──────────────────────────────────┤
│ L3 · cold · global traits        │  holds across every project
└──────────────────────────────────┘
```

One binary is everything: the HTTP service, the CLI, the agent skill
installer, and the LLM-backed sync pipeline. Rust or not, projects use it
without depending on it.

---

## 1. Run it

**Try it temporarily (nothing installed, nothing leaks):**

```bash
git clone <this-repo> && cd tiered-memory
scripts/tm help            # builds on first run, data goes to /tmp scratch
scripts/tm clean           # wipes the test data when you're done
```

**Install properly:**

```bash
cargo install --path .                     # → ~/.cargo/bin/tiered-memory
# add --features local for the embedded LLM vectorizer (real semantic
# embeddings in-process; downloads a ~90 MB model once)
```

Data lives in `TM_DATA_DIR` (default `~/tiered-memory`) — everything the tool
ever writes is under that one directory; delete it and memory is gone.

## 2. Set up LLM credentials (needed for `sync`)

```bash
tiered-memory credentials set
```

Interactive terminal wizard: pick a provider (OpenAI / OpenRouter / Groq /
Ollama / LM Studio / vLLM, or a custom URL) → masked API-key input → it
**fetches the provider's live model list** and you type-to-search it. Saved to
`{data}/credentials.json` with `0600` perms.

Scripts can skip the TUI: `credentials set --base-url … --api-key … --model …`
(`tiered-memory models` lists the catalog; `credentials show` is masked).
Env vars `TM_LLM_BASE_URL` / `TM_LLM_API_KEY` / `TM_LLM_MODEL` also work.

## 3. Add a project

Run inside any project directory — Rust, Node, Python, anything:

```bash
cd my-project/
tiered-memory init
```

It detects the project (package.json / Cargo.toml / pyproject.toml / dir
name), registers it, and writes a `tiered-memory.json` marker so every later
command resolves the project automatically.

## 4. Use it

```bash
tiered-memory remember "In this project the learner wants pure theory" --param code_example_density=0
tiered-memory remember "Learner is strong in Python" --global          # → L3 trait
tiered-memory recall "how should I introduce recursion?"
tiered-memory params                                                   # adjusted parameters, per layer
tiered-memory projects                                                 # everything using tiered memory
tiered-memory select                                                   # pick the current project
tiered-memory stats                                                    # L1/L2/L3 counts vs capacity
```

Inside an init'ed project the `--project` flag is unnecessary. Parameters
(`--param key=value`, repeatable) beat prose: they merge deterministically —
**nearest layer wins** (L1 project → L2 related → L3 global), conflicts show
up as `alternatives` in `params`.

## 5. Let your agent update memory: the `/tiered-memory` skill

```bash
tiered-memory install-skill        # → ~/.agents/skills/tiered-memory/SKILL.md
```

Then, in any harness that loads skills, invoke **`/tiered-memory`** after a
working session. The agent gathers all three layers, pipes the transcript
into the sync pipeline, and reports what landed:

```bash
tiered-memory sync --stdin <<'EOF'
<session transcript>
EOF
# gathers L1+L2+L3 → one LLM extraction call → writes each update into the
# right layer; re-running is safe (updates are upserts)
# add --dry-run to preview without writing
```

## 6. Or drive it over HTTP (for apps)

```bash
tiered-memory serve        # http://127.0.0.1:7900
```

| Endpoint | Purpose |
|---|---|
| `GET /v1/health` · `GET /v1/stats/{user}` | status, per-layer counts |
| `POST /v1/projects` · `GET /v1/projects/{user}` | register/list projects |
| `POST /v1/remember` · `POST /v1/recall` | store · layered semantic search |
| `POST /v1/params` · `POST /v1/feedback` | adjusted parameters · assert one value |
| `POST /v1/consolidate` · `POST /v1/forget` · `POST /v1/reindex` | maintenance |

Full request/response shapes: [docs/API.md](docs/API.md). Zero-dependency
Node client: [clients/node/tiered-memory.mjs](clients/node/tiered-memory.mjs).
CLI writes route through a running service automatically, so the CLI and
`serve` never disagree.

### Auth

Loopback + no token by default (personal-machine threat model — see
[docs/SECURITY_ANALYSIS.md](docs/SECURITY_ANALYSIS.md)). For shared machines:
put a secret in `$TM_DATA_DIR/token` (`head -c 32 /dev/urandom | base64 > token`)
and every route except `/v1/health` requires `Authorization: Bearer <token>`.
Non-loopback binds without a token are refused unless `TM_ALLOW_INSECURE=1`.

## Where your data lives

The directory tree *is* the cache — JSON is authoritative, `.md` files are
human-readable mirrors regenerated on every write:

```text
~/tiered-memory/
├── current-project                    ← written by `select`
├── credentials.json                   ← LLM credentials (0600)
├── projects/<id>.json                 ← project descriptors
├── cache/
│   ├── L1/<project>/memories.json|md  ← hot, project-only
│   ├── L2/memories.json|md            ← related scopes (one big file)
│   │   └── similar-projects.txt       ← hand-editable project links
│   └── L3/memories.json|md            ← global user traits
└── users/<other>/…                    ← additional users (server mode)
```

## Configuration

| Variable | Default | Meaning |
|---|---|---|
| `TM_DATA_DIR` | `~/tiered-memory` | data root |
| `TM_BIND` | `127.0.0.1:7900` | HTTP bind address |
| `TM_USER` | `local` | default user |
| `TM_EMBEDDER` | `hashing` | `hashing` \| `local` \| `http` |
| `TM_MODEL` | `minilm` | local preset (`minilm` \| `bge-small`) |
| `TM_L1/L2/L3_CAPACITY` | `128/1024/4096` | layer sizes |
| `TM_CONSOLIDATE_EVERY` | `50` | auto-consolidate every N writes |
| `TM_BASE_URL` | `http://127.0.0.1:7900` | service URL the CLI talks to |
| `TM_LLM_BASE_URL/API_KEY/MODEL` | – | LLM credentials via env |

Embedder backends: `hashing` (offline, lexical, default), `local` (embedded
candle sentence-transformer, CPU, no Python), `http` (any OpenAI-compatible
`/embeddings` endpoint). Switching is safe — stores carry a fingerprint and
refuse mixed geometries until `POST /v1/reindex`.

## As a Rust crate (optional)

```toml
tiered-memory = { path = "../tiered-memory" }   # or { git = "…" }
```

```rust
use std::sync::Arc;
use tiered_memory::{MemoryEngine, EngineConfig, LayeredDirStore, EmbedderConfig};
let engine = MemoryEngine::new(
    Arc::new(LayeredDirStore::new(tiered_memory::default_data_dir())?),
    EmbedderConfig::default().build()?,
    EngineConfig::default(),
);
```

`cargo run --example basic` is a two-minute library tour.

## Docs & tests

- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) — cache policies, invariants, trade-offs
- [docs/API.md](docs/API.md) — HTTP reference
- [docs/INTEGRATION.md](docs/INTEGRATION.md) — wiring hosts (incl. shell-out pattern)
- [docs/SECURITY_ANALYSIS.md](docs/SECURITY_ANALYSIS.md) — threat model + findings

```bash
cargo test                                # 41 tests, offline
cargo test --features local -- --ignored  # + real-model smoke test
```

MIT — see [LICENSE](LICENSE).
