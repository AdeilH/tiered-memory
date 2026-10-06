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
installer, and the LLM-backed sync pipeline.

## Install

```bash
cargo install --path .
```

Data lives in `~/tiered-memory` (override with `TM_DATA_DIR`); delete it and
memory is gone.

## Quick start

```bash
cd my-project/
tiered-memory setup          # register the project, pick an L2 group,
                             # configure the LLM, install the agent skill
```

That's it. Or do the steps individually:

```bash
tiered-memory init                       # register this project
tiered-memory credentials                # LLM provider for `sync`
tiered-memory install-skill              # install the /tiered-memory agent skill
```

## Everyday commands

```bash
tiered-memory remember "Learner wants pure theory" --param code_example_density=0
tiered-memory remember "Learner is strong in Python" --global      # → L3
tiered-memory recall "how should I introduce recursion?"
tiered-memory params                   # adjusted parameters, per layer
tiered-memory status                   # setup report
tiered-memory stats                    # L1/L2/L3 counts vs capacity
tiered-memory console                  # terminal dashboard
tiered-memory forget <id>              # or: --project P | --level L3 | --all
```

Memories merge deterministically — **nearest layer wins** (L1 → L2 → L3);
conflicts show up as `alternatives` in `params`.

## Let your agent update memory

```bash
tiered-memory install-skill            # interactive picker of detected harnesses
tiered-memory install-hooks            # session start/end hooks (where supported)
```

Then, after a working session, invoke **`/tiered-memory`** in your harness:
the agent pipes the transcript through the LLM sync pipeline and writes each
update into the right layer. Re-running is safe (upserts); add `--dry-run` to
preview.

## Serve over HTTP (optional)

```bash
tiered-memory serve        # http://127.0.0.1:7900 — root is a browser dashboard
```

Endpoints cover remember/recall/params, projects, groups, and maintenance —
see [docs/API.md](docs/API.md). Binds to loopback only; `tiered-memory auth on`
adds token auth.

## Try it without installing

```bash
git clone <this-repo> && cd tiered-memory
scripts/tm help            # builds on first run, data goes to /tmp scratch
scripts/tm clean           # wipes the test data
```

## Docs

- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) — cache policies, invariants, trade-offs
- [docs/API.md](docs/API.md) — HTTP reference
- [docs/INTEGRATION.md](docs/INTEGRATION.md) — wiring hosts (incl. shell-out pattern)
- [docs/SECURITY_ANALYSIS.md](docs/SECURITY_ANALYSIS.md) — threat model + findings
- `tiered-memory help` — every command, incl. groups, cross-project `use`, bench

MIT — see [LICENSE](LICENSE).
