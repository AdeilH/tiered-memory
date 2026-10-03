---
name: tiered-memory
description: Layered long-term memory (L1/L2/L3 cache-style) for the learner, powered by the tiered-memory binary. Use when the user invokes /tiered-memory, asks to "update memory", "remember this", "sync memory", "what do you remember about me", or wants their preferences/traits gathered across all three layers and written back after a working session. Gathers L1 (current project), L2 (related scopes), L3 (global traits), extracts new knowledge with an OpenAI-compatible LLM, and updates every layer.
---

# Tiered Memory

You maintain the learner's **layered long-term memory** through the
`tiered-memory` binary. Like a CPU cache: **L1** is hot and project-local,
**L2** is warm and covers related scopes (sibling components, similar
projects), **L3** is the cold, global layer of user-level traits shared by
every project.

## Commands (the binary is already installed)

```bash
tiered-memory params   # adjusted parameters for the current project
tiered-memory recall "query"              # layer-annotated search
tiered-memory projects                    # every project using tiered memory
tiered-memory stats                       # per-layer counts vs capacity
```

If the working directory has a `tiered-memory.json`, the project is resolved
from it automatically; otherwise pass `--project <id>` (run `tiered-memory
projects` to list them, or `cd` into the project and `tiered-memory init`).

## `/tiered-memory` — update everything

When the user invokes this skill, run the full update pass:

1. **Gather.** Read what is currently held in all three layers:

   ```bash
   tiered-memory params
   tiered-memory recall "<summarize what this session was about>" --k 8
   tiered-memory stats
   ```

2. **Update.** Pipe the recent conversation (the transcript, or a faithful
   rendering of what happened in this session) into the sync pipeline:

   ```bash
   tiered-memory sync --stdin <<'EOF'
   <paste the relevant conversation/session content here>
   EOF
   ```

   The binary gathers L1+L2+L3 itself, sends them plus the conversation to the
   configured OpenAI-compatible LLM, and writes every extraction back into the
   right layer (L1 project-local, L2 related scopes, L3 global traits).
   Re-asserting a parameter key updates it — updates are upserts, so running
   sync repeatedly is safe.

3. **Report.** Summarize for the user, one line per stored memory, grouped by
   layer, e.g.:
   - `L1 · In this project the learner wants pure theory, no code examples`
   - `L3 · Learner is strong in Python, beginner in Rust systems topics`

   Mention anything the model skipped and why. Show the resulting `tiered-memory params` when parameters changed.

### When no LLM is configured

`tiered-memory sync` needs credentials (`tiered-memory credentials show`).
If none are set, either ask the user to run:

```bash
tiered-memory credentials set --base-url https://api.openai.com/v1 \
  --api-key sk-... --model gpt-4o-mini        # any OpenAI-compatible provider
```

…or fall back to doing the routing yourself: from the gathered state, decide
for each new fact which layer owns it, then store it directly:

```bash
tiered-memory remember "In this project the learner wants pure theory"   # → L1 (this project)
tiered-memory remember "Learner is strong in Python" --global            # → L3 (global trait)
tiered-memory remember "Prefers zustand on the frontend" --level L2      # explicit layer
```

## Routing rules (when you store memories yourself)

- **L1** — only true for THIS project ("wants pure theory here"). Default for
  project-scoped `remember`.
- **L2** — matters to related scopes: sibling components of the same product
  (frontend ↔ backend) or similar projects.
- **L3** — durable user-level traits ("strong in Python", "prefers games
  analogies everywhere"). Use `--global`.
- Prefer **parameters** over prose when a value is tunable:
  `tiered-memory remember "..." --param difficulty=0.4 --param pace=slow`
  (repeatable; numbers/true-false/text auto-typed). Parameters beat prose
  because they merge deterministically at read time — nearest layer wins.

## Parameters view

`tiered-memory params` prints the adjusted parameter set for the current
project: every learned value with its source layer and confidence, conflicts
shown as alternatives. Surface these when teaching/tutoring so the session
actually reflects the learner.

## Data

Everything lives under `TM_DATA_DIR` (default `~/tiered-memory`):
`cache/L1/<project>/memories.md`, `cache/L2/` (with hand-editable
`similar-projects.txt`), `cache/L3/` — human-readable mirrors are regenerated
on every write. Pinned memories survive eviction; TTLs expire on
consolidation.
