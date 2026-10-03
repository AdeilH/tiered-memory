# Integrating tiered-memory into an application

tiered-memory is deliberately **not bound to any host project**. Two ways in:

1. **HTTP service** (language-agnostic) — run `tiered-memory-server`, talk JSON.
2. **Rust library** — depend on the crate and call `MemoryEngine` directly.

This guide covers the HTTP path, with a zero-dependency Node client in
[`clients/node/tiered-memory.mjs`](../clients/node/tiered-memory.mjs).

## Running the service

```bash
cd tiered-memory

# production-ish: embedded local model, release build
cargo run --release --features local -- serve

# dev, offline (hashing embedder — lexical similarity, no model download):
cargo run -- serve
```

Data lives at `$TM_DATA_DIR` (default `~/tiered-memory`) in the layered layout
(`cache/L1/<project>/`, `cache/L2/`, `cache/L3/` — see README). The default
user is `local`.

Model download happens once (≈90 MB, cached under the data dir); after that
everything is local CPU inference.

## The integration pattern

A tutoring/chat host only needs four touch points:

| Moment | Call | Why |
|---|---|---|
| App/project registers (or intake form) | `POST /v1/projects` | descriptor enables L2 similarity links |
| Session starts | `POST /v1/params` | get the adjusted parameter set for this learner+project |
| Learner acts / gives feedback | `POST /v1/feedback` | capture parameter updates ("too fast", picked option A…) |
| Before composing a prompt | `POST /v1/recall` | pull relevant memories as context, layer-annotated |

Occasionally (or every N writes — the server also auto-consolidates):
`POST /v1/consolidate`.

## Node.js example

```js
import { TieredMemory } from './clients/node/tiered-memory.mjs';

const mem = new TieredMemory('http://127.0.0.1:7900');

// one-time per project: describe it so L2 similarity can link related work
await mem.registerProject({
  user: 'adeel',
  project_id: 'teacher',
  name: 'Teacher AI Skill Studio',
  components: ['frontend', 'backend'],
});

// session start: defaults merged under the learner's adjustments
const { params, detail } = await mem.params({
  user: 'adeel',
  project_id: 'teacher',
  defaults: { difficulty: 0.5, lesson_style: 'standard', analogy_domain: 'auto' },
});
// params = { difficulty: 0.35, lesson_style: 'standard', analogy_domain: 'games' }
// detail.difficulty.source === 'L1'  → project-local override won

// tutor graded a checkpoint and the learner asked for more depth:
await mem.feedback({ user: 'adeel', project_id: 'teacher', key: 'difficulty', value: 0.4 });

// before teaching, pull memories as context:
const { hits } = await mem.recall({
  user: 'adeel', project_id: 'teacher',
  query: 'how should I introduce recursion?',
});
const context = hits.map(h => `[${h.level}] ${h.text}`).join('\n');
```

### Prompt injection (recommended shape)

```
Learner profile (from memory):
- difficulty: 0.35 (project preference; global says 0.65 — project wins)
- analogy_domain: games (global trait)
- relevant: [L1] In this project the learner wants pure theory, no code examples
```

## Wiring up the `teacher` project (concrete sketch)

The tutor server (`server/src/tutor/session.js` builds the system prompt) would:

1. On tutor chat start: `params` with the course brief's own settings as
   `defaults` (lesson_style from intake, track, metaphor-style). The returned
   map overrides/augments the brief with *learned* values.
2. Feed the returned values into the tutor system prompt as a
   "Learner profile" section (shape above).
3. On signals: checkpoint failures → `feedback difficulty -=`; "✦ analogy"
   requests → bump analogy preference; explicit settings changes → `feedback`.
4. Skill id = `project_id`; register each generated skill with its topic as
   descriptor so related courses share L2.

Because the memory layer is a separate service, the web UI never touches it
and the teacher server only grows a ~30-line client module.

## Embedder choice

- Start with `local` (MiniLM): real semantics, free, private, in-process.
- Use `http` if you already run an embeddings provider or want one shared
  store served by e.g. Ollama.
- Keep `hashing` for CI/e2e tests — deterministic and dependency-free.

**Never mix embedders on one data dir** without reindexing: the store
fingerprint (HTTP 409) will stop you instead of corrupting recall.

## Operational notes

- Data is `$TM_DATA_DIR/*.json` — one file per user, atomic writes, trivially
  backup-able. Back it up like any other user data.
- Consolidation runs automatically every `TM_CONSOLIDATE_EVERY` writes; also
  call it after import bursts.
- All state changes are local to the user file; per-user locks, so concurrent
  requests for different users don't contend.
- No auth in v1 — bind to `127.0.0.1` or put a proxy in front.
