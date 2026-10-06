# tiered-memory — command reference

Every `tiered-memory` subcommand, verified against the CLI implementation
(`src/bin/tiered-memory/`). See `README.md` for concepts, `docs/ARCHITECTURE.md`
for the L1/L2/L3 model, `docs/INTEGRATION.md` for embedding it in an app.

**Project resolution** — most commands pick their project from, in order:
`--project P` → the `tiered-memory.json` marker in the current directory
(written by `init`) → the last `select` choice. **User** defaults to `local`
(`--user U` or `TM_USER` anywhere).

**Layer routing on write** — explicit `--level` wins; `--global` → L3;
`--group NAME` → L2 (group-owned); default → L1 of the resolved project.

## Memory

### `remember`
```bash
tiered-memory remember "text" [--project P] [--level L1|L2|L3] [--global]
                   [--group NAME] [--topic TOPIC] [--param k=v]...
                   [--pin] [--ttl DAYS] [--user U]
```
Store a memory (a cache line). Routing: default L1 of the project; `--level L2`
project-owned but surfaced to related scopes; `--group NAME` L2 owned by the
whole group (cannot combine with `--global`/`--project`); `--global` → L3.
`--topic` is an L2-only category slug that files the record into
`cache/L2/groups/<group>/<topic>.md` (examples: `writing-style`, `contracts`).
`--param k=v` is repeatable and accepts numbers/bools/text; `--ttl DAYS` sets
an expiry. Writes go through the running service when one responds, else the
local store. Prints `stored <id> (deduped: true|false)`.

```bash
tiered-memory remember "Prefers clap derive" --level L2 --topic preferences
tiered-memory remember "API returns camelCase" --group myapp --topic contracts
tiered-memory remember "scratch note" --ttl 7 --pin
```

### `feedback`
```bash
tiered-memory feedback <key> <value> [--global] [--project P] [--user U]
```
Assert one learner parameter — the agent-facing signal API. Values coerce to
number/bool/text. Re-asserting the same key at the same scope updates in place.

```bash
tiered-memory feedback pace 0.4
tiered-memory feedback error_style terse --global
```

### `recall`
```bash
tiered-memory recall "query" [--project P] [--k N] [--min F] [--user U]
```
Layered search: probes L1 → L2 → L3 (nearer layers win ties via level weight),
deduplicates promoted copies, touches what it serves, and promotes L2/L3 hits
used ≥ 3 times into the project's L1 (write-allocate). `--k` caps hits
(default 6); `--min F` is the cosine floor.

### `params`
```bash
tiered-memory params [--project P] [--user U]
```
Show the adjusted parameter set: for each key, the nearest layer wins; deeper
disagreeing values appear as `alternatives`.

### `forget`
```bash
tiered-memory forget <id> | forget --project P | forget --level L1|L2|L3
                     | forget --all [--yes]
```
Hard delete (no audit log — that's the point). Only `--all` asks for
confirmation (y/N in a TTY; `--yes` required in scripts) — targeted forgets
delete immediately.

### `stats`
```bash
tiered-memory stats [--user U]
```
Per-layer record counts vs capacity.

## Project lifecycle

### `init` / `setup`
```bash
tiered-memory init [--name N] [--id ID] [--descriptor T]
                   [--tag T]... [--component C]...
                   [--group NAME|none] [--user U] [--gitignore]
```
Register THIS directory as a project and write the `tiered-memory.json`
marker. Detects name/description from `package.json`/`Cargo.toml`/
`pyproject.toml`/dir name. `--tag`/`--component` are repeatable and feed the
descriptor (similar-project matching). Then the first-run wizard: asks the L2
group (`--group NAME`, or `--group none` to pre-answer "no group"), offers LLM
credentials, offers skill install. `--gitignore` pre-answers the .gitignore
prompt. Safe to re-run to update name/descriptor; never wipes an existing
group or `uses` links.

### `projects`
```bash
tiered-memory projects [--user U]
tiered-memory projects remove <id> [--user U]
```
List registered projects (with similar/uses links), or unregister one — drops
its records at every level and strips links pointing at it.

### `select`
```bash
tiered-memory select [--project P] [--user U]
```
Pick the current project (interactive when `P` is omitted). Backs the global
`current-project` marker.

### `status`
```bash
tiered-memory status [--user U] [--check]
```
One-glance setup report: service reachable?, data dir, project resolution, L2
group, LLM credentials. `--check` exits 1 when this directory isn't set up —
for `status --check || init` in shell profiles.

## L2 sharing (groups + uses)

### `group`
```bash
tiered-memory group [--project P] [--user U]      # show (+ suggestion when unset)
tiered-memory group set <name|none>               # assign / confirm "no group"
tiered-memory group rename <old> <new>            # move projects + group-owned
                                                  # memories; renaming onto an
                                                  # existing group MERGES the two
```
An L2 group is a user-confirmed family of projects that share warm memories:
every member sees every other member's project-owned L2 records plus
group-owned records. L1 stays per-project. The reserved name `none` records an
explicit "belongs to no group" confirmation (the ask-once flow stops asking).

### `use`
```bash
tiered-memory use                                 # show sources + who draws on me
tiered-memory use <other>                         # this project draws on <other>
tiered-memory use --remove <other>
```
Directional memory borrowing: the project now also sees `<other>`'s L1 **and**
L2 memories, serving from its warm (L2) tier; the reverse gains nothing.
Hot borrowed lines promote into the borrower's L1 like any L2 hit; `--remove`
stops new borrowing but leaves already-promoted copies cached. Both projects
must be registered. Links live in `cache/uses.txt` (hand-editable for
not-yet-registered projects).

## LLM sync + credentials

### `sync`
```bash
tiered-memory sync [--file F | --text T | --stdin] [--project P] [--user U]
                   [--dry-run]
```
Gather all three layers + adjusted parameters, extract new/changed knowledge
with the configured LLM, write it back through the normal `remember` path.
`--dry-run` prints the plan without writing. Needs credentials (`credentials`).

### `credentials`
```bash
tiered-memory credentials                 # interactive: provider, key, live model list
tiered-memory credentials [--base-url U] [--api-key K] [--model M]   # scripted
tiered-memory credentials show | clear
```
Stored at `{data}/credentials.json` (mode 0600). Also settable via `TM_LLM_*`
env vars.

### `models`
```bash
tiered-memory models
```
List the configured provider's models (live search).

### `auth`
```bash
tiered-memory auth on | off | show
```
Bearer-token auth for the HTTP service (token lives at `{data}/token`).

## Harness integration

### `install-skill`
```bash
tiered-memory install-skill [--harness <id,id>] [--dir D] [--subcommands] [--list]
```
Install the `/tiered-memory` agent skill into your harness(es). `--list` shows
known harnesses; `--subcommands` adds completable `/tiered-memory:*` skills.

### `install-hooks`
```bash
tiered-memory install-hooks [--harness <id,id>] [--remove]
```
Wire memory into session start (injects the learner brief: adjusted params +
L1/L2/L3 context + group) and session end (syncs the transcript).

### `hook`
```bash
tiered-memory hook session-start | session-end
```
The command the hooks invoke — rarely called by hand.

## Service & ops

### `serve`
```bash
tiered-memory serve        # HTTP service on 127.0.0.1:7900 (TM_BIND to change)
```
Full request/response shapes: **[docs/API.md](docs/API.md)**. Auth
(`tiered-memory auth on`) requires `Authorization: Bearer <token>` on every
call when enabled.

| Method | Path | Purpose |
|---|---|---|
| GET  | `/` | browser dashboard (read-only, self-refreshing) |
| GET  | `/v1/health` | version, embedder fingerprint, dims, user count |
| GET  | `/v1/stats/{user}` | per-layer counts vs capacity, param keys |
| GET  | `/v1/context/{user}` | L3-only gather (no project scope) |
| GET  | `/v1/context/{user}/{project}` | learner brief: L1+L2+L3 lines, params, group + members |
| POST | `/v1/projects` | register a project (name, tags, components, descriptor, group) |
| POST | `/v1/projects/group` | assign the project's L2 group (`"none"` confirms no group, `null` resets) |
| POST | `/v1/projects/group/rename` | rename (or merge) a group across projects + group-owned memories |
| POST | `/v1/projects/uses` | add/remove a directional `uses` memory source (`{"add"|"remove": other}`) |
| GET  | `/v1/projects/{user}` | list registered projects with links |
| DELETE | `/v1/projects/{user}/{project}` | unregister a project + forget its records |
| POST | `/v1/remember` | store a memory (full `RememberInput`: kind, params, key_hint, level, group, topic, confidence, pin, ttl) |
| POST | `/v1/recall` | layered search (`k`, `min_similarity`, `write_allocate`) |
| POST | `/v1/params` | adjusted parameters merged over the host's `defaults`, with per-key detail |
| POST | `/v1/feedback` | assert one parameter (`key`, typed `value`, `global`, `weight`) |
| POST | `/v1/consolidate` | expire/forget/merge + trait lift → consolidation report |
| POST | `/v1/forget` | hard delete by id / project / level / all |
| POST | `/v1/reindex` | re-embed every vector under the current embedder (migration after a model switch) |

Errors are `{"error": "..."}` with 400/401/404/409/500 (409 = embedder
fingerprint mismatch — the cue to `reindex`).

### `env`
```bash
eval "$(tiered-memory env)"
```
Print exports (PATH + TM_DATA_DIR) for the current shell.

### `console`
```bash
tiered-memory console [--user U]
```
Terminal dashboard: layer gauges, projects, L2 groups with per-topic docs,
installed skills, project↔group graph.

### `bench`
```bash
tiered-memory bench [--projects N] [--per-project N] [--queries N]
                    [--embedder hashing|local|http] [--keep] [--json]
```
Benchmark write/recall/consolidate on a synthetic corpus in a scratch store —
real data is never touched. Compare environments via `TM_EMBEDDER`/`TM_DATA_DIR`.

### `clean`
```bash
tiered-memory clean [--skills | --data] [--yes]
```
Remove everything tiered-memory created (data dir incl. credentials, installed
skill copies). Asks unless `--yes`.

## Environment

| Variable       | Default              | Purpose                                   |
|----------------|----------------------|-------------------------------------------|
| `TM_DATA_DIR`  | `~/tiered-memory`    | data root                                 |
| `TM_STORE`     | `layered`            | `layered` (cache/L1\|L2\|L3) or `flat`    |
| `TM_USER`      | `local`              | default user                              |
| `TM_EMBEDDER`  | `local` (default build; `hashing` in `--no-default-features` builds) | `hashing` \| `local` \| `http`  |
| `TM_BIND`      | `127.0.0.1:7900`     | `serve` bind address                      |
| `TM_BASE_URL`  | `http://127.0.0.1:7900` | service URL the CLI prefers            |
| `TM_L1_CAPACITY` | `128`              | per-project hot-line capacity             |
| `TM_L2_CAPACITY` | `1024`             | shared warm-tier capacity (all projects)  |
| `TM_L3_CAPACITY` | `4096`             | global trait capacity                     |
| `TM_CONSOLIDATE_EVERY` | `50`         | writes between auto-consolidations (0 = never) |
| `TM_WRITE_ALLOCATE` | `1`            | promote hot L2/L3 hits into L1 (0/1)      |
| `TM_MIN_SIMILARITY` | embedder default | recall cosine floor                     |
| `TM_LLM_*`     | —                    | LLM provider/key/model for `sync`         |

## Recipes

```bash
# set up a project
cd myapp && tiered-memory init

# backend + frontend sharing one warm tier (L2), independent hot lines (L1)
cd myapp-backend   && tiered-memory init --component backend  --group myapp
cd ../myapp-frontend && tiered-memory init --component frontend --group myapp

# monolith + microservices, one family
cd shop-payments && tiered-memory init --component backend --group shop

# product-wide fact (no single owner)
tiered-memory remember "all services auth via gateway token" --group myapp --topic contracts

# agent asserts a parameter; user tunes the store
tiered-memory feedback pace 0.4
tiered-memory recall "how should I explain recursion?"

# session hygiene
tiered-memory status --check || tiered-memory init
tiered-memory forget --level L1           # clear this project's hot line
```
