---
name: tiered-memory
description: Layered long-term memory (L1/L2/L3 cache-style) for the learner, powered by the tiered-memory binary. Use when the user invokes /tiered-memory, asks to "update memory", "remember this", "sync memory", "what do you remember about me", or when a tutoring/teaching session starts and ends. At session start it loads the learner profile (adjusted parameters + relevant memories); during the session the AGENT stores durable learner signals itself via remember/feedback; at session end it runs the sync pass that gathers all three layers, extracts new knowledge with an OpenAI-compatible LLM, and updates every layer.
---

# Tiered Memory

You maintain the learner's **layered long-term memory** through the
`tiered-memory` binary. Like a CPU cache: **L1** is hot and project-local,
**L2** is warm and covers related scopes (sibling components, similar
projects), **L3** is the cold, global layer of user-level traits shared by
every project.

**You call the commands — the learner never does.** The learner talks to you;
you decide what is worth remembering and run the commands yourself.

If the working directory has a `tiered-memory.json`, the project is resolved
from it automatically; otherwise pass `--project <id>` (run `tiered-memory
projects` to list them, or `cd` into the project and `tiered-memory init`).

## Session protocol

### 1. Session start — load the profile

Run these immediately, before teaching anything:

```bash
tiered-memory params                                    # adjusted parameters per layer
tiered-memory recall "<what this session is about>" --k 5   # relevant memories
```

Shape the session from what comes back: difficulty, pace, analogy domain,
prior knowledge. Empty output means a new learner — calibrate by asking a
couple of questions, then store what you learn (step 2).

### 2. During the session — capture signals as they happen

Whenever something durable surfaces, store it **immediately** — one command,
then keep teaching. Don't wait for the learner to ask, and don't batch it all
for the end.

```bash
# project-local preference with parameters (→ L1)
tiered-memory remember "In this course the learner wants pure theory, no code examples" \
  --param code_example_density=0

# parameter updates (→ L1; add --global for cross-project values → L3)
tiered-memory feedback difficulty 0.25
tiered-memory feedback pace slow --global

# durable trait about the learner (→ L3)
tiered-memory remember "Learner is strong in TypeScript, beginner in Rust" --global

# explicit layer for related-scope knowledge (→ L2)
tiered-memory remember "Prefers zustand on the frontend" --level L2
```

Signal → command mapping (tutoring signals, but the pattern generalizes):

| Signal | Store |
|---|---|
| checkpoint failed, learner confused | `feedback difficulty` (lower it ~0.1) |
| checkpoint passed quickly and easily | `feedback difficulty` (raise slightly) |
| "no code", "too much theory", "slower" | `remember` the preference + matching `--param` |
| likes a particular analogy domain | `feedback analogy_domain <domain> --global` |
| mentions strong prior knowledge | `remember "strong in X" --global` |
| mentions their goal ("interview prep") | `remember` it — goal belongs in L3 |

Rules: store only **durable** facts (never one-off questions or session
noise); prefer `--param` over prose when the thing is a tunable; L3 only for
traits that clearly hold across projects; never store credentials or secrets
from the conversation.

### 3. Session end (or on request) — the full sync pass

The transcript-level extraction: gather all three layers, send them plus the
session to the configured OpenAI-compatible LLM, and let it catch anything
steps 1–2 missed.

```bash
tiered-memory sync --stdin <<'EOF'
<the session transcript>
EOF
```

Re-running is safe (updates are upserts). Add `--dry-run` to preview. Report
the result to the learner one line per stored memory, grouped by layer, and
show the resulting `tiered-memory params` when parameters changed.

### When no LLM is configured

`sync` needs credentials (`tiered-memory credentials show`). If none are set,
either ask the user to run:

```bash
tiered-memory credentials
```

which opens an interactive terminal setup: pick a provider (OpenAI, OpenRouter,
Groq, Ollama, LM Studio, vLLM or a custom URL), enter the API key (masked),
and then **search the provider's live model list** (fetched from its
`/models` endpoint; type-to-filter, arrow keys, Enter). Flags
(`--base-url/--api-key/--model`) skip the TUI for scripts.

…or skip `sync` entirely — steps 1 and 2 above need no LLM and already make
the memory learn: gather the layers yourself, route new facts with the rules
above, and store them with `remember` / `feedback`.

## Commands reference

```bash
tiered-memory params                     # adjusted parameters for this project
tiered-memory recall "query"             # layer-annotated search
tiered-memory remember "text" [--param k=v]… [--global | --level L2] [--pin]
tiered-memory feedback <key> <value> [--global]
tiered-memory projects                   # every project using tiered memory
tiered-memory stats                      # per-layer counts vs capacity
```

## Data

Everything lives under `TM_DATA_DIR` (default `~/tiered-memory`):
`cache/L1/<project>/memories.md`, `cache/L2/` (with hand-editable
`similar-projects.txt`), `cache/L3/` — human-readable mirrors are regenerated
on every write. Pinned memories survive eviction; TTLs expire on
consolidation.
