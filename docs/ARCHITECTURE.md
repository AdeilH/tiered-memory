# Architecture

The engine is a cache hierarchy over *learner memories* with a derived
*parameter view* on top. This document explains the data model, the policies,
and the reasoning behind the trade-offs.

## 1. Data model

### Levels (`src/types.rs`)

```
enum Level { L1, L2, L3 }          // derived order: L1 < L2 < L3 (depth)
```

| Level | Scope | Intuition |
|---|---|---|
| `L1` | one `project_id` | the lines the learner is actively using |
| `L2` | a `project_id`, *surfaced to* related scopes | warm lines from sibling components and similar projects |
| `L3` | user-global | traits; what a brand-new project starts with |

`Level::weight()` (1.0 / 0.9 / 0.85) biases ranking so a hit from a nearer
layer beats an equally-similar deeper one.

### MemoryRecord

A memory is a cache line: natural-language `text`, its embedding `vector`, the
`level` it lives at, optional `project_id`, a `kind`
(`preference | trait | feedback | summary | note`), optional structured
`params: {key → ParamValue}`, plus the bookkeeping the policies need:
`confidence` (0..1), `pinned`, `created_at_ms` / `last_used_at_ms` /
`use_count`, `origin` (for promoted copies), `expires_at_ms` (TTL).

Two identities matter:

- `canonical_id = origin ?? id` — a promoted L1 copy and its L2/L3 original are
  the *same* memory; recall dedupes on this and never serves both.
- `key_hint` — for parameter-carrying records (feedback, lifted traits), the
  canonical parameter key. Re-asserting the same parameter at the same scope
  *updates in place* instead of allocating a new line (see §3).

### ParamValue

`number | text | bool` (untagged JSON). Compatibility (used for consensus and
for de-duplicating alternatives): numbers agree within a tolerance (default
0.15), text/bool must match case-insensitively.

### UserDb

Everything for one learner: embedder fingerprint + dims, the records, and the
project registry (`ProjectInfo`: descriptor text + vector, tags, components,
`similar: Vec<project_id>`, `uses: Vec<project_id>`).

## 2. The Embedder boundary

```
trait Embedder: Send + Sync {
    fn name(&self) -> &'static str;      // fingerprint component
    fn dims(&self) -> usize;
    fn default_min_similarity(&self) -> f32;
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;  // L2-normalized
}
```

Contract: vectors are **L2-normalized** (cosine ≡ dot), deterministic per
(config, text), and `fingerprint() = "name:dims"` is persisted with the store.

Why the fingerprint matters: embeddings from different models are
incomparable. Mixing geometries silently would poison recall, so the engine
refuses to operate on a store whose fingerprint differs from the running
embedder (`MemoryError::EmbedderMismatch`, HTTP 409) and offers `reindex` as
the migration path. This turns "changed the model" from a corruption bug into
a deliberate, checkable operation.

Backends: `hashing` (FNV-hashed word uni-grams + char 3-grams, sublinear TF —
collision noise floor ≈ 0.15 cosine at 512 dims), `local` (candle BertModel,
CLS or masked-mean pooling, presets for MiniLM-L6 / bge-small; weights are
safetensors loaded in-process, fetched once from the hub and cached), `http`
(OpenAI-compatible `/embeddings`, dims probed on first call or configured).

Embedding happens **outside locks** in every engine path.

## 3. Write path (`remember`, `feedback`)

1. **Embed** the text.
2. **Route** to a level: explicit `level` wins; otherwise `kind == trait` or
   no project → `L3`; else `L1` of the given project. (`L1` requires a
   `project_id`.)
3. **Upsert by key**: same `key_hint` at the same (level, scope) → update
   text/vector/params, blend confidence `(a+b)/2`, bump `use_count`, refresh
   timestamps. Parameter re-assertions are updates, not new lines.
4. **Dedup by content**: otherwise, a same-scope record with cosine >
   `dup_threshold` (0.96) merges in place (confidence
   `1-(1-a)(1-b)` capped 0.99).
5. **Allocate**: otherwise push a fresh record.
6. **Capacity** (see §5) and save.
7. Every `auto_consolidate_every` writes (default 50) triggers consolidation.

`feedback(user, key, value, project_id|global, weight)` is sugar over
`remember`: text `"{key} = {value}"`, `params {key: value}`, `key_hint = key`,
kind `feedback`, confidence `weight`.

## 4. Read path (`recall`)

1. Embed the query; resolve visibility:
   - `L1` — records of exactly this project **plus**, when the project
     explicitly `uses` another one, that project's L1 records too. Borrowed
     hot lines do not serve as the viewer's hot line: a record's
     *serving level* is L2 whenever an L1 record serves for a project other
     than its owner (`MemoryRecord::serving_level_for`), so "L1 in the
     output = this project's hot line" stays true everywhere — ranking,
     parameter precedence, and the sync gather's L1/L2 buckets alike.
   - `L2` — records of this project **plus** records of related scopes:
     `similar` projects (linked at registration by descriptor similarity),
     the project's **L2 group** (a user-confirmed family of projects, e.g.
     `rust-clis`; membership lives in `ProjectInfo.group` + the hand-editable
     `groups.txt`), the projects this one explicitly `uses`, and group-owned
     records (`group` set on the record itself, no single owning project —
     "all my CLIs use clap"). Same-project components share the project id,
     so they federate automatically.
   - `L3` — everything global.
2. Score every visible, unexpired record:
   `score = cosine × level_weight × (0.5 + 0.5·confidence) × (1 + 0.15·2^(−age_days/14))`
   — similarity dominates; layer, confidence and freshness nudge ties.
3. Filter `cosine ≥ min_similarity` (request > config > embedder default),
   sort, dedupe by `canonical_id`, take `k`.
4. **Touch** served records (`use_count += 1`, `last_used = now`) — this feeds
   promotion, eviction and the parameter ranker.
5. **Write-allocate**: hits from L2/L3 whose `use_count ≥ promote_min_hits`
   (default 3) are copied into L1 of the current project (`origin` set,
   capacity enforced, originals untouched). Hot knowledge migrates toward the
   learner; L1 stays hot precisely because cold lines don't get promoted.
6. Persist the touches/promotions.

`adjusted_parameters(user, project_id)` is the parameter view over the same
visibility rules without a query: for each key, rank candidates by
`level_weight × confidence × 2^(−age_days/half_life) × (1 + 0.1·ln(1+use))`
(default half-life 45 d). Nearest layer wins the key; deeper disagreeing
values are returned as `alternatives` (best-first, deduped). Because this is
derived, not stored, there is no state to corrupt and conflicts stay visible.

`parameters_with_defaults` merges the host app's baseline under the learner's
adjustments → one ready-to-apply map.

## 5. Eviction & promotion (cache management)

`enforce_capacity(level)`: while a level is over capacity, evict the lowest-
value unpinned record, where

```
value = confidence × 2^(−age_days/30) × (1 + ln(1+use_count)) × (key_hint ? 1.25 : 1)
```

- **L1 evictees that are canonical** (`origin == None`) are **demoted to L2**
  (write-back) — project knowledge is never silently destroyed by capacity.
- Promoted copies evicted from L1 are simply dropped; their originals live on
  deeper.
- L2/L3 overflow drops the lowest-value line (they had their chance); pinned
  lines are never evicted (L1 may temporarily exceed capacity if everything
  in it is pinned).

## 6. Consolidation (`consolidate`, auto every N writes)

1. **Expire** TTLs (`expires_at_ms` past; pinned immune) → `report.expired`.
2. **Forget** `confidence < confidence_floor` (0.05) → `report.forgotten`.
3. **Merge** same-(level, scope) near-duplicates (cosine > `merge_threshold`,
   0.92; records with different `key_hint`s never merge): newer survives,
   confidence combines `1-(1-a)(1-b)`, params union (newer wins), counters sum.
4. **Trait lift**: for each parameter key asserted in L1/L2 across ≥
   `trait_lift_min_projects` (2) *distinct* projects, compute the consensus
   value (all numbers within tolerance → mean; text/bool must agree exactly).
   Agreement → upsert an L3 `trait` record ("Across N projects, the learner
   consistently sets key = value", confidence `min(0.97, 0.5 + 0.15·N)`).
   Disagreement → left alone; layer precedence keeps per-project values local.
   This is how "always wants slow pace" stops being re-taught per project.
5. **Capacity** across all layers.

## 7. Similar projects, groups, and `uses` (L2 federation)

`register_project` embeds `descriptor` (explicit text or `name + tags +
components`) and recomputes pairwise similarity over all of the user's
projects; pairs with cosine ≥ `similar_project_threshold` (0.5) get linked
symmetrically in `ProjectInfo.similar`. Recall for project P expands L2
visibility to `similar(P)`. Re-registration (a project "changes topic")
recomputes links. Components ("frontend"/"backend") are metadata *and*
descriptor terms; since they live under one project id, they always share L2.

Two user-controlled mechanisms sit on top:

- **L2 groups** (`ProjectInfo.group` + `groups.txt`) — a symmetric family:
  every member sees every other member's project-owned L2 records, plus
  group-owned records (`group` on the record itself).
- **`uses`** (`ProjectInfo.uses` + `cache/uses.txt`, managed by
  `tiered-memory use`) — directional and explicit: `b.uses = [a]` surfaces
  a's L1 *and* L2 records in b (at b's warm tier, see §4), while a gains
  nothing. Hot borrowed lines promote into b's L1 through normal
  write-allocate; unlinking (`use --remove`) stops new borrowing but leaves
  already-promoted copies in b's hot line, like any cached line. Both
  projects must be registered; links survive re-registration and are
  stripped when the used project is removed.

## 8. Storage

`trait MemoryStore { load(user) / save(user, db) / users() }`. The engine
caches loaded users in memory
(`RwLock<HashMap<user, Arc<Mutex<UserDb>>>>`), locks per user for the duration
of a mutation (embedding happens outside), and saves after each mutation.

### LayeredDirStore (default) — the cache hierarchy as directories

```
{root}/                              ← user `local` (standalone default)
  meta.json                          version + embedder fingerprint
  current-project                    CLI selection marker
  projects/<project-id>.json         ProjectInfo (descriptor, tags, similar, uses)
  cache/
    uses.txt                         project → project memory sources (hand-editable)
    L1/<project-id>/memories.json    records at L1 of that project (machine)
    L1/<project-id>/memories.md      regenerated human-readable mirror
    L2/memories.json                 all L2 records (flat machine store)
    L2/groups/<g>/<topic>.md         human docs per group + topic
    L2/ungrouped/<topic>.md          …for projects without a group
    L2/groups.txt                    project → group membership (hand-editable)
    L2/similar-projects.txt          project link pairs
    L3/memories.json|md              user-level traits
{root}/users/<other-user>/…          additional users
```

Design rules:

- **JSON is authoritative; MD is a mirror.** Ids, vectors and timestamps are
  not human-editable data, so the machine record stays JSON (pretty-printed,
  atomic tmp+rename). Each write regenerates the layer's `memories.md` (L2:
  the per-topic files) — read, grep and diff them freely; edits go through
  the API/CLI so the two never drift silently.
- **The relationship files are the authoritative human files.**
  `similar-projects.txt` holds one symmetric pair per line (`projectA
  projectB`); `uses.txt` one directional pair per line (`using used`).
  `save` writes the union of (whatever the file already contained) and (the
  engine-side links), so hand-added lines survive recomputation. On `load`,
  lines merge into the projects; a pair referencing a not-yet-registered
  project attaches to the known side and activates when the other project
  registers. Two unknown ids is the only case that drops. (`groups.txt` is
  stricter: the file wins on load outright — clear an assignment with
  `none`, not by deleting the line.)
- **Reconciliation on save**: stale project files and L1 directories of
  removed projects are deleted; every registered project gets an L1 folder
  (empty rather than missing) so the tree always mirrors the registry.
- All writes go through one `atomic_write` (tmp + rename). Scale envelope:
  thousands of records per user → linear scan is sub-ms; replace the store
  (SQLite/redb) and/or add an ANN index when that stops being true.

### JsonFileStore (flat alternative)

One `{root}/{user}.json` per user, selected with `TM_STORE=flat`. Kept for
simple embeddable setups and tests; same trait, same engine.

## 9. Deliberate limitations (v1)

- Brute-force search, no ANN index (see §8 scale envelope).
- Single-process; no cross-process locking over the data dir. Coherence
  between the CLI and a running service is preserved by policy instead:
  CLI *write* commands (`init`, `remember`) post to the service when it
  responds and only fall back to direct store writes when no service is
  reachable. Read commands always load fresh from disk.
- HTTP auth is one shared bearer token (opt-in via `{data}/token`) — sized for
  a personal standalone binary on loopback, not for multi-tenant serving.
- Similar-project links are pairwise, not clustered multi-membership.
- MD mirrors are write-only from the engine's perspective: hand edits to
  `memories.md` are overwritten (edit via API/CLI; `similar-projects.txt` is
  the hand-editable exception).
- No audit log; `forget` is a hard delete (it's the point).

## 10. LLM-assisted sync (`tiered-memory sync`)

The `/tiered-memory` skill drives a three-step pipeline (lib: `sync::plan` +
`sync::apply`, so callers can preview or route the writes elsewhere):

1. **Gather** — `MemoryEngine::memory_context` collects the visible lines of
   all three layers (L1 of the project, L2 of related scopes, L3 global) plus
   the adjusted parameters, capped per layer, vectors stripped.
2. **Extract** — one chat completion against any OpenAI-compatible endpoint
   (`LlmClient`; credentials from `{data}/credentials.json` written 0600, or
   `TM_LLM_*` env, or an explicit `--credentials` file). The system prompt
   restates the layer semantics, shows the current state so known facts are
   not re-asserted, and demands a strict JSON `{"updates": […]}` reply.
3. **Apply** — every update goes through the engine's normal `remember` path
   (explicit level, typed `params`, optional `key`): key upserts, content
   dedupe, capacity and persistence all apply unchanged. Unknown levels and
   empty texts are dropped at plan time; failures surface as `skipped`.

Because extraction is a normal LLM call, the routing quality is bounded by the
model, but the engine stays the source of truth: the LLM can only propose
memories, and every invariant (visibility, precedence, capacity, expiry) is
enforced by the same code paths as direct writes. The CLI applies through the
running service when reachable (same HTTP-first policy as `remember`).
