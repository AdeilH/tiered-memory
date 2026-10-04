# HTTP API reference

All bodies are JSON. Errors are `{"error": "..."}` with 400 (invalid input),
404 (unknown user/memory/project), 409 (embedder fingerprint mismatch —
reindex), 500 (storage/embedder failure).

---

## `GET /v1/health`

```json
{ "ok": true, "version": "0.1.0", "embedder": "minilm:384", "dims": 384, "users": 3 }
```

## `GET /v1/stats/{user}`

```json
{
  "user": "adeel",
  "embedder": "minilm:384",
  "dims": 384,
  "counts":  { "l1": 12, "l2": 34, "l3": 8 },
  "capacities": { "l1": 128, "l2": 1024, "l3": 4096 },
  "projects": 4,
  "keys": ["analogy_domain", "difficulty", "pace"]
}
```

## `POST /v1/projects`

```json
{
  "user": "adeel",
  "project_id": "teacher",
  "name": "Teacher AI Skill Studio",
  "tags": ["tutoring", "llm"],
  "components": ["frontend", "backend"],
  "descriptor": "optional explicit descriptor text; defaults to name+tags+components",
  "group": "tutors"                     // optional L2 group; omit to keep an existing one
}
```
→ the stored `ProjectInfo` (including computed `similar: [...]`). Re-posting
the same `project_id` updates it and recomputes links.

## `POST /v1/projects/group`

Assign the project's **L2 group** — the user-confirmed membership the agent
skill asks about once per project. Group members see each other's L2 memories.

```json
{ "user": "adeel", "project_id": "teacher", "group": "tutors" }
```
`"group": "none"` records an explicit no-group confirmation (the skill stops
asking); `"group": null` resets to unassigned. → the stored `ProjectInfo`.

## `POST /v1/projects/group/rename`

Rename an L2 group everywhere at once — every assigned project and every
group-owned memory moves. Renaming onto an existing group **merges** the two
(the fix for split families like `rustcli` vs `rust-clis`).

```json
{ "user": "adeel", "from": "rustcli", "to": "rust-clis" }
```
→ `{"projects": 2, "records": 1}` (400 when `from` matches nothing, `to` is
reserved/invalid, or both names are equal).

## `POST /v1/projects/uses`

Link one project's memory into another — the explicit, **directional**
cross-project reuse (groups share warm memories symmetrically; `uses` makes
*this* project draw on one *specific* project, hot lines included). Exactly
one of `add` / `remove`:

```json
{ "user": "adeel", "project_id": "student", "add": "teacher" }
```

`student` now sees `teacher`'s L1 **and** L2 memories — serving from
student's warm L2 tier (its own L1 still wins), with hot lines migrating
into student's L1 via write-allocate as they keep being recalled.
`teacher` gains nothing. `{"remove": "teacher"}` drops the link; both
projects must be registered (404 otherwise), `add` is idempotent.
→ the stored `ProjectInfo` with the updated `uses: [...]`.

## `GET /v1/projects/{user}`

→ array of `ProjectInfo`.

## `DELETE /v1/projects/{user}/{project}`

Unregisters the project and forgets all of its records (every level), and
drops similarity links pointing at it → `3` (bare count of forgotten
memories; 404 if unknown). The store reconciles the project's L1 folder and
link entries on the next save.

## `GET /v1/context/{user}` · `GET /v1/context/{user}/{project}`

The gathered `MemoryContext` for a scope — L1/L2/L3 lines (no vectors) plus
the adjusted parameter set with per-key provenance. This is what `sync`
gathers and what the session-start hook renders into the learner brief.
Without a project, only L3 is visible. Group-scoped L2 visibility applies
(same rules as `recall`).

## `POST /v1/remember`

```json
{
  "user": "adeel",
  "text": "In this project the learner wants pure theory, no code examples",
  "project_id": "teacher",              // optional; required for L1
  "kind": "preference",                 // preference|trait|feedback|summary|note (optional)
  "params": { "code_example_density": 0.0 },   // optional structured assertions
  "key_hint": "code_example_density",   // optional; upsert-by-key at this scope
  "confidence": 0.8,                    // optional 0..1 (default 0.8)
  "pinned": false,                      // optional
  "ttl_days": 30,                       // optional
  "level": "L1",                        // optional explicit placement; default routing:
                                        //   trait or no project → L3, else L1
  "group": "rust-clis",                 // optional; L2 record owned by the whole group
                                        //   ("all my CLIs use clap") — implies level L2,
                                        //   visible to every member project
  "topic": "writing-style"              // optional category slug; files the L2 mirror
                                        //   into <group>/<topic>.md (free text is
                                        //   normalized, default: general)
}
```
→ `{"id": "m…", "deduped": false, "demoted_to_l2": 0, "auto_consolidated": false}`

## `POST /v1/recall`

```json
{
  "user": "adeel",
  "query": "how should the tutor explain garbage collection?",
  "project_id": "teacher",     // optional; when present L1+L2 are probed
  "k": 6,                      // optional
  "min_similarity": 0.30,      // optional; default per embedder
  "write_allocate": true       // optional; default from config
}
```
→
```json
{
  "hits": [
    {
      "id": "m…", "text": "…", "similarity": 0.71, "score": 0.68,
      "level": "L1", "kind": "preference", "project_id": "teacher",
      "params": {"difficulty": 0.35}, "confidence": 0.9,
      "use_count": 4, "created_at_ms": 1790000000000
    }
  ],
  "searched": ["L1", "L2", "L3"],
  "promoted": []
}
```

## `POST /v1/params`

```json
{
  "user": "adeel",
  "project_id": "teacher",      // optional; absent → L3 only
  "defaults": { "difficulty": 0.5, "lesson_style": "standard" }
}
```
→
```json
{
  "params":   { "difficulty": 0.35, "analogy_domain": "games", "lesson_style": "standard" },
  "detail": {
    "difficulty": {
      "key": "difficulty", "value": 0.35, "source": "L1",
      "confidence": 0.9, "updated_at_ms": 1790000000000,
      "alternatives": [ { "value": 0.65, "source": "L3", "confidence": 0.8 } ]
    }
  }
}
```
`params` is the ready-to-apply map (defaults merged under learner values);
`detail` carries provenance and conflicts per key.

## `POST /v1/feedback`

```json
{ "user": "adeel", "project_id": "teacher",
  "key": "difficulty", "value": 0.35, "weight": 0.9 }
// or globally:  { "user": "adeel", "key": "analogy_domain", "value": "games", "global": true }
```
→ same shape as `remember` (upserts when the same key is asserted again at the
same scope).

## `POST /v1/consolidate`

`{"user": "adeel"}` →
`{"expired": 1, "forgotten": 0, "merged": 2, "traits_lifted": 1}`

## `POST /v1/forget`

```json
{ "user": "adeel", "id": "m…" }                                  // one memory (+ its copies)
{ "user": "adeel", "project_id": "teacher" }                     // all of a project
{ "user": "adeel", "project_id": "teacher", "level": "L1" }      // filtered
{ "user": "adeel", "all": true }                                 // everything
```
→ `3` (bare count of removed memories; 404 if a given `id` was unknown)

## `POST /v1/reindex`

`{"user": "adeel"}` → re-embeds all records + descriptors with the *running*
embedder and updates the store fingerprint → `42` (bare count of re-embedded
memories)
