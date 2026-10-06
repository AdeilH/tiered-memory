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
│                                  │  + projects this one uses
├──────────────────────────────────┤
│ L3 · cold · global traits        │  holds across every project
└──────────────────────────────────┘
```

One binary is everything: the HTTP service, the CLI, the agent skill
installer, and the LLM-backed sync pipeline. Rust or not, projects use it
without depending on it.

---

## 1. Run it

**Set up (recommended):** run inside any project directory and answer a few
short prompts — project registration, its L2 group (the family of projects
it shares memories with), LLM provider (skipped if already configured), and
which agent harness(es) get the skill:

```bash
cd my-project/
tiered-memory setup
```

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

## 2. Set up the LLM (needed for `sync`)

One OpenAI-compatible provider config powers everything — memory extraction
(`sync`), the model catalog (`models`), and the `http` embedder:

```bash
tiered-memory credentials
```

Interactive terminal wizard: pick a provider (OpenAI / OpenRouter / Groq /
Ollama / LM Studio / vLLM, or a custom URL) → masked API-key input → it
**fetches the provider's live model list** and you type-to-search it. Saved to
`{data}/credentials.json` with `0600` perms.

Scripts can skip the TUI: `credentials set --base-url … --api-key … --model …`.
Env vars `TM_LLM_BASE_URL` / `TM_LLM_API_KEY` / `TM_LLM_MODEL` also work.

## 3. Add a project

Run inside any project directory — Rust, Node, Python, anything:

```bash
cd my-project/
tiered-memory init
```

It detects the project (package.json / Cargo.toml / pyproject.toml / dir
name), registers it, and writes a `tiered-memory.json` marker so every later
command resolves the project automatically. It shows the full paths (marker +
memory data dir) and asks whether to add the marker to `.gitignore` — Enter
defaults to yes; `--gitignore` pre-answers for scripts.

## 4. Use it

```bash
tiered-memory remember "In this project the learner wants pure theory" --param code_example_density=0
tiered-memory remember "Learner is strong in Python" --global          # → L3 trait
tiered-memory remember "All my CLI projects use clap" --group rust-clis  # → L2, group-owned
tiered-memory recall "how should I introduce recursion?"
tiered-memory params                                                   # adjusted parameters, per layer
tiered-memory group                                                    # this project's L2 group
tiered-memory group rename <old> <new>                                 # rename a group (onto an existing one = merge)
tiered-memory use <other-project>                                      # draw on another project's L1+L2
tiered-memory status                                                   # setup report (--check: exit 1 if not set up)
tiered-memory projects                                                 # everything using tiered memory
tiered-memory select                                                   # pick the current project
tiered-memory stats                                                    # L1/L2/L3 counts vs capacity
tiered-memory forget <id>   # or: forget --project P | --level L3 | --all
tiered-memory console                                                  # terminal dashboard (see §6)
```

Inside an init'ed project the `--project` flag is unnecessary. Parameters
(`--param key=value`, repeatable) beat prose: they merge deterministically —
**nearest layer wins** (L1 project → L2 related → L3 global), conflicts show
up as `alternatives` in `params`.

**L2 groups.** Projects can be assigned to a named group (a family like
`rust-clis` or `tutors`) — group members share L2 memories. Run
`tiered-memory group` to see the assignment (+ a suggestion and the existing
groups when unset). The `/tiered-memory` skill asks about it **once per
project**; the interactive picker lists existing groups to join by number —
or founds a new one, optionally seeding it with other projects, so the name
is just a label for a membership you can see. `group set <name|none>`
records the answer (authoritative — automatic clustering only proposes) and
guards against typos: a new name within edit distance of an existing group
asks for confirmation. `group rename <old> <new>` fixes or merges groups
after the fact, moving every project and group-owned memory.

**Cross-project reuse.** Groups share warm memories symmetrically; sometimes
you want one project to draw on *one specific* project — its hot L1 lines
included. `tiered-memory use <other-project>` links this project to that one:
its L1 **and** L2 memories surface here in the warm (L2) tier, and hot ones
migrate into this project's L1 as they keep being recalled (write-allocate).
The link is **directional** — the other project gains nothing — and
`tiered-memory use` shows both directions (what this project uses, and who is
drawing on it); `use --remove <other>` drops the link. All of this also
works over HTTP (`POST /v1/projects/uses`).

**L2 docs, not one big file.** L2's human-readable layer is filed per group
and per topic: `cache/L2/groups/rust-clis/writing-style.md`,
`flow.md`, `preferences.md`, … Tag memories when writing them
(`remember … --level L2 --topic writing-style`) or let the sync LLM pick the
category; free text is normalized to a lowercase slug and records without one
default to `general.md`. (Groupless projects' L2 records land in
`ungrouped/<topic>.md`; the JSON beside it stays the machine record.)

## 5. Let your agent update memory: the `/tiered-memory` skill

Every harness keeps skills somewhere different. `install-skill` knows the
common targets, detects what's on your machine, and installs with a
multi-select picker:

```bash
tiered-memory install-skill            # interactive picker (detected first)
tiered-memory install-skill --list     # the registry + install status
tiered-memory install-skill --harness claude,agents   # scriptable
tiered-memory install-skill --dir ~/.anywhere/skills  # custom directory
```

Built-in targets: the Agent Skills spec (`~/.agents/skills`, used by ZCode and
friends), Claude Code (user + project scope), and instruction blocks for
harnesses that read project rules instead: Codex CLI (`AGENTS.md`), Junie
(`.junie/guidelines.md`), Aider (`CONVENTIONS.md`), Cline/Roo (`.clinerules`),
Windsurf (`.windsurf/rules/`). OpenCode and Gemini CLI are best-effort skill
dirs. **Any harness not on the list is two lines away** — see below.

**Adding your own harness.** Edit `harnesses.json` in the data dir
(`~/tiered-memory/harnesses.json`, or `$TM_DATA_DIR/harnesses.json`):

```json
{
  "harnesses": [{
    "id": "commandcode",
    "label": "CommandCode",
    "target": ".commandcode/skills",
    "mode": "skill-dir",
    "scope": "user",
    "detect": [".commandcode"]
  }]
}
```

`mode` is `skill-dir` (copy the skill into `<target>/tiered-memory/`) or
`agents-md` (append the marked instruction block to the target file — works
for any rules/guidelines markdown). `scope` anchors the path at `$HOME`
(`user`) or the project (`project`). Custom entries show up everywhere the
built-ins do: the picker, `install-skill --list` (marked `[custom]`),
`clean`, and the console.

**Subcommand completion.** Harnesses autocomplete skill *names*, not
arguments — `/tiered-memory sync the session` passes free text to the model.
To get real completion, install the subcommand family:
`install-skill --subcommands` (the picker asks too). It adds thin sibling
skills — `/tiered-memory-sync`, `-recall`, `-remember`, `-group`, `-params` —
so typing `/tiered-memory` narrows the harness's completion to all of them,
each with instructions for its command.

**Hooks — memory without invoking anything.** Harnesses that support hooks
can fire tiered-memory automatically:

```bash
tiered-memory install-hooks            # detected harnesses (claude, ZCode)
tiered-memory install-hooks --harness claude --remove
```

Session start injects the learner brief (parameters, group, recent memories)
into context; session end (Claude Code) syncs the transcript through the LLM
pipeline. ZCode has no session-end event — end-of-session capture there stays
with the `/tiered-memory` skill.

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

## 6. Watch it work: the console

```bash
tiered-memory console
```

A read-only terminal dashboard (ratatui, five panels): layer gauges with
capacity, the project table with per-project L1/L2 counts, L2 groups with
their per-topic docs, every installed skill copy with its summary — and a
graph drawing projects on the left, L2 groups on the right, and a line for
each membership. `tab`/`←`/`→` switch panels, `r` refreshes, `q` quits.

## 7. Or drive it over HTTP (for apps)

```bash
tiered-memory serve        # http://127.0.0.1:7900
```

Every request and operation is logged to stdout (`remember`/`recall`/`feedback`
outcomes with dedupe and promotion counts, deletions, consolidation results,
errors — health probes skipped). `TM_QUIET=1` silences it.

The service root is a **browser dashboard**: layer gauges, every project as a
node, L2 groups on the right, a line per membership — open
`http://127.0.0.1:7900/` while it runs (`?user=<name>` for non-default
users). For the terminal version with more detail, see `tiered-memory console`.

| Endpoint | Purpose |
|---|---|
| `GET /v1/health` · `GET /v1/stats/{user}` | status, per-layer counts |
| `POST /v1/projects` · `GET /v1/projects/{user}` | register/list projects |
| `POST /v1/projects/group` · `POST /v1/projects/group/rename` · `POST /v1/projects/uses` | L2 group · rename/merge · cross-project memory sources |
| `POST /v1/remember` · `POST /v1/recall` | store · layered semantic search |
| `POST /v1/params` · `POST /v1/feedback` | adjusted parameters · assert one value |
| `POST /v1/consolidate` · `POST /v1/forget` · `POST /v1/reindex` | maintenance |

Full request/response shapes: [docs/API.md](docs/API.md). Zero-dependency
Node client: [clients/node/tiered-memory.mjs](clients/node/tiered-memory.mjs).
CLI writes route through a running service automatically, so the CLI and
`serve` never disagree.

### Auth

Off by default — the service binds to loopback only, which is the right
threat model for a personal binary (analysis in
[docs/SECURITY_ANALYSIS.md](docs/SECURITY_ANALYSIS.md)). If you ever expose it:

```bash
tiered-memory auth on      # generates a token (0600), prints it once
tiered-memory auth show    # or: off
```

then restart `serve` — every route except `/v1/health` now requires
`Authorization: Bearer <token>` (the Node client accepts it as a second
constructor argument). Non-loopback binds refuse to start without a token
unless `TM_ALLOW_INSECURE=1`.

## Where your data lives

The directory tree *is* the cache — JSON is authoritative, `.md` files are
human-readable mirrors regenerated whenever the layer's content changes:

```text
~/tiered-memory/
├── current-project                    ← written by `select`
├── credentials.json                   ← LLM credentials (0600)
├── projects/<id>.json                 ← project descriptors
├── cache/
│   ├── uses.txt                       ← hand-editable project → project memory sources
│   ├── L1/<project>/memories.json|md  ← hot, project-only
│   ├── L2/memories.json               ← related scopes (flat machine store)
│   │   ├── groups/<g>/<topic>.md      ← per-group, per-topic docs
│   │   │                                 (writing-style.md, flow.md, …)
│   │   ├── ungrouped/<topic>.md       ← L2 of projects without a group
│   │   ├── groups.txt                 ← hand-editable group membership
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
| `TM_EMBEDDER` | `local` | `local` \| `hashing` \| `http` (no `local` feature → `hashing`) |
| `TM_MODEL` | `minilm` | local preset (`minilm` \| `bge-small`) |
| `TM_L1/L2/L3_CAPACITY` | `128/1024/4096` | layer sizes |
| `TM_CONSOLIDATE_EVERY` | `50` | auto-consolidate every N writes |
| `TM_BASE_URL` | `http://127.0.0.1:7900` | service URL the CLI talks to |
| `TM_LLM_BASE_URL/API_KEY/MODEL` | – | LLM credentials via env |

Embedder backends: `local` (embedded candle sentence-transformer, CPU, no
Python — the default, `minilm:384`, weights fetched once from the hub and
cached), `hashing` (offline, lexical — the fallback in builds without the
`local` feature), `http` (any OpenAI-compatible `/embeddings` endpoint —
**shares the same credentials as `sync`**, no separate config). Switching is
safe — stores carry a fingerprint and refuse mixed geometries until
`POST /v1/reindex`.

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

## Benchmark it

```bash
tiered-memory bench                                  # defaults: 40 projects, 8 memories each
tiered-memory bench --projects 200 --queries 500 --json > hash.json
TM_EMBEDDER=local tiered-memory bench --json > local.json   # embedded model
TM_DATA_DIR=/dev/shm/tm tiered-memory bench ...             # tmpfs vs disk
```

Generates a deterministic synthetic corpus (same numbers on every machine —
no LLM, no network), runs register / remember / recall-hit / recall-miss /
params / consolidate against a **scratch store that is deleted afterwards** —
your real memory is never touched. `--keep` keeps the scratch store,
`--store DIR` benchmarks an existing directory instead. Compare tables across
`TM_EMBEDDER` backends, tmpfs vs disk, or machine generations; `--json` is
for scripted diffs. Debug builds print a warning — benchmark release builds
only.

## Uninstall

```bash
tiered-memory clean              # removes the data dir (~/tiered-memory —
                                 # memories AND LLM credentials) and every
                                 # installed agent skill copy; asks first
cargo uninstall tiered-memory    # the binary itself
```

Per-project marker files (`tiered-memory.json`) are left in place — delete
them per project if you want a spotless machine.
