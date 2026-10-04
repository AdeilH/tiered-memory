//! `tiered-memory bench` — a self-contained benchmark over a deterministic
//! synthetic corpus. No LLM, no network: the same workload runs anywhere, so
//! numbers are comparable across environments — flip `TM_EMBEDDER`
//! (hashing | local | http), point `TM_DATA_DIR` at tmpfs vs disk, and
//! compare the tables.
//!
//! The benchmark always writes to a **scratch store** (a fresh temp dir,
//! removed afterwards) so it can never touch real memory. `--store DIR`
//! opts into benchmarking an existing directory instead (it will gain
//! synthetic projects named `bench-*`).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::{arg_switch, arg_value, cmd_args};
use tiered_memory::{
    EmbedderConfig, EngineConfig, Level, MemoryEngine, ProjectInput, RecallInput, RememberInput,
};

// -- deterministic corpus -----------------------------------------------------

/// Word pools per project family. Projects from the same family share
/// vocabulary, so descriptor-similarity linking and group semantics have
/// real structure to work with; the off-topic pool powers miss queries.
const DOMAINS: [&[&str]; 4] = [
    &[
        "rust", "cli", "argument", "parsing", "terminal", "binary", "cargo", "release",
    ],
    &[
        "react",
        "frontend",
        "dashboard",
        "components",
        "styling",
        "browser",
        "state",
        "routing",
    ],
    &[
        "python",
        "training",
        "dataset",
        "pipeline",
        "inference",
        "tensor",
        "gpu",
        "eval",
    ],
    &[
        "docker",
        "deploy",
        "kubernetes",
        "infra",
        "scripts",
        "monitoring",
        "logs",
        "ci",
    ],
];
const STYLES: &[&str] = &[
    "concise answers",
    "worked examples",
    "deep theory first",
    "code-first walkthroughs",
    "analogies from games",
    "step-by-step checklists",
];
const OFF_TOPIC: &[&str] = &[
    "gardening",
    "recipes",
    "soccer",
    "piano",
    "hiking",
    "pottery",
    "astronomy",
    "beekeeping",
];

/// Tiny deterministic LCG — same seed, same corpus, on every machine.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }
    fn pick<'a, T>(&mut self, slice: &'a [T]) -> &'a T {
        &slice[(self.next() % slice.len() as u64) as usize]
    }
}

/// The generated workload: what to register, what to write, what to ask.
struct Corpus {
    projects: Vec<(String, String)>, // (id, descriptor)
    /// (project_id, text, level, difficulty param?)
    memories: Vec<(String, String, Level, Option<f64>)>,
    /// Queries phrased from corpus vocabulary — expected to hit.
    queries_hit: Vec<(String, String)>, // (project_id, query)
    /// Queries from unrelated vocabulary — expected to miss (full scan).
    queries_miss: Vec<String>,
}

impl Corpus {
    fn generate(projects: usize, per_project: usize) -> Self {
        let mut rng = Lcg(0x5EED_2026);
        let mut out = Corpus {
            projects: Vec::new(),
            memories: Vec::new(),
            queries_hit: Vec::new(),
            queries_miss: Vec::new(),
        };
        for i in 0..projects {
            let domain = DOMAINS[i % DOMAINS.len()];
            let descriptor: Vec<&str> = (0..5).map(|_| *rng.pick(domain)).collect();
            let id = format!("bench-{:03}", i);
            out.projects.push((id.clone(), descriptor.join(" ")));

            // per-project memories: mostly L1, a couple of L2, one parameter
            for m in 0..per_project {
                let style_a = *rng.pick(STYLES);
                let style_b = *rng.pick(STYLES);
                let (level, text) = match m % 4 {
                    3 => (
                        Level::L2,
                        format!(
                            "sibling projects prefer {} over {} ({id} note {m})",
                            style_a, style_b
                        ),
                    ),
                    _ => (
                        Level::L1,
                        format!(
                            "in {id} the learner prefers {style_a} for {domain_word} work (note {m})",
                            domain_word = rng.pick(domain)
                        ),
                    ),
                };
                out.memories.push((id.clone(), text, level, None));
            }
            out.memories.push((
                id.clone(),
                format!("difficulty for {id}"),
                Level::L1,
                Some(0.1 + (i % 9) as f64 * 0.1),
            ));

            // a hit query per project, phrased from its own vocabulary
            out.queries_hit.push((
                id.clone(),
                format!("does the learner prefer {} here?", rng.pick(STYLES)),
            ));
            // a global trait every few projects
            if i % 3 == 0 {
                out.memories.push((
                    String::new(),
                    format!(
                        "the learner consistently leans {} across projects",
                        rng.pick(STYLES)
                    ),
                    Level::L3,
                    None,
                ));
            }
        }
        for q in 0..(projects.max(1) * 2) {
            let a = OFF_TOPIC[q % OFF_TOPIC.len()];
            let b = OFF_TOPIC[(q * 3 + 1) % OFF_TOPIC.len()];
            out.queries_miss.push(format!("{a} versus {b} planning"));
        }
        out
    }
}

// -- measurement --------------------------------------------------------------

#[derive(Default)]
struct Scenario {
    name: &'static str,
    samples: Vec<Duration>,
}

impl Scenario {
    fn new(name: &'static str) -> Self {
        Scenario {
            name,
            samples: Vec::new(),
        }
    }
    fn time<F: FnMut()>(&mut self, mut f: F) {
        let start = Instant::now();
        f();
        self.samples.push(start.elapsed());
    }
    fn total(&self) -> Duration {
        self.samples.iter().sum()
    }
    fn sorted(&self) -> Vec<Duration> {
        let mut s = self.samples.clone();
        s.sort();
        s
    }
    fn percentile(&self, p: usize) -> Duration {
        let s = self.sorted();
        let idx = (s.len() * p / 100).min(s.len().saturating_sub(1));
        s.get(idx).copied().unwrap_or_default()
    }
    fn mean_ms(&self) -> f64 {
        if self.samples.is_empty() {
            return 0.0;
        }
        self.total().as_secs_f64() * 1000.0 / self.samples.len() as f64
    }
    fn mean(&self) -> Duration {
        if self.samples.is_empty() {
            return Duration::ZERO;
        }
        self.total() / self.samples.len() as u32
    }
    fn ops_per_s(&self) -> f64 {
        let t = self.total().as_secs_f64();
        if t <= 0.0 {
            0.0
        } else {
            self.samples.len() as f64 / t
        }
    }
}

fn fmt_ms(d: Duration) -> String {
    let ms = d.as_secs_f64() * 1000.0;
    if ms >= 100.0 {
        format!("{ms:.0}ms")
    } else if ms >= 1.0 {
        format!("{ms:.2}ms")
    } else {
        format!("{:.0}µs", ms * 1000.0)
    }
}

// -- command ------------------------------------------------------------------

struct BenchConfig {
    projects: usize,
    per_project: usize,
    queries: usize,
    store: Option<PathBuf>,
    keep: bool,
    json: bool,
}

pub(crate) fn bench_cmd() -> Result<(), String> {
    let args = cmd_args();
    let cfg = BenchConfig {
        projects: arg_value(&args, "--projects")
            .and_then(|v| v.parse().ok())
            .unwrap_or(40),
        per_project: arg_value(&args, "--per-project")
            .and_then(|v| v.parse().ok())
            .unwrap_or(8),
        queries: arg_value(&args, "--queries")
            .and_then(|v| v.parse().ok())
            .unwrap_or(200),
        store: arg_value(&args, "--store").map(PathBuf::from),
        keep: arg_switch(&args, "--keep"),
        json: arg_switch(&args, "--json"),
    };
    if cfg.projects == 0 {
        return Err("--projects must be >= 1".into());
    }

    // the embedder decides most of the runtime — time its load separately
    // (with `local` this is the model download/load, seconds by nature)
    let load_start = Instant::now();
    let embedder = embedder_from_args(&args)?;
    let load = load_start.elapsed();

    // scratch store unless the user explicitly points somewhere. /tmp is
    // world-writable, so the name must be unguessable and must not follow a
    // pre-planted symlink — nanos + pid + refuse-if-exists (see
    // SECURITY_ANALYSIS.md, v0.2 additions)
    let scratch;
    let store_dir = match &cfg.store {
        Some(dir) => dir.clone(),
        None => {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0);
            let dir = std::env::temp_dir().join(format!(
                "tiered-memory-bench-{}-{:x}",
                std::process::id(),
                nanos
            ));
            if dir.exists() {
                return Err(format!(
                    "bench scratch dir already exists: {} — remove it or pass --store",
                    dir.display()
                ));
            }
            scratch = dir;
            scratch.clone()
        }
    };

    let corpus = Corpus::generate(cfg.projects, cfg.per_project);
    let memory_count = corpus.memories.len();
    // honor --queries: split the budget across hit and miss scenarios
    let mut corpus = corpus;
    let half = (cfg.queries / 2).max(1);
    corpus.queries_hit.truncate(half);
    corpus.queries_miss.truncate(cfg.queries - half);

    let store = tiered_memory::LayeredDirStore::new(&store_dir).map_err(|e| e.to_string())?;
    let engine = MemoryEngine::new(
        std::sync::Arc::new(store),
        embedder,
        EngineConfig::from_env(),
    );

    // -- scenarios ------------------------------------------------------------
    let mut register = Scenario::new("register");
    for (id, descriptor) in &corpus.projects {
        let id = id.clone();
        let descriptor = descriptor.clone();
        register.time(|| {
            engine
                .register_project(ProjectInput {
                    user: USER.into(),
                    project_id: id.clone(),
                    name: Some(id.clone()),
                    tags: vec![],
                    components: vec![],
                    descriptor: Some(descriptor.clone()),
                    group: None,
                })
                .expect("bench register");
        });
    }

    let mut remember = Scenario::new("remember");
    for (project, text, level, difficulty) in &corpus.memories {
        let project = project.clone();
        let text = text.clone();
        let level = Some(*level);
        let params = difficulty
            .map(|d| {
                std::collections::BTreeMap::from([(
                    "difficulty".to_string(),
                    tiered_memory::ParamValue::Number(d),
                )])
            })
            .map(Box::new);
        remember.time(|| {
            engine
                .remember(RememberInput {
                    user: USER.into(),
                    text: text.clone(),
                    project_id: if project.is_empty() {
                        None
                    } else {
                        Some(project.clone())
                    },
                    kind: None,
                    params: params.as_deref().cloned(),
                    key_hint: None,
                    confidence: None,
                    pinned: None,
                    ttl_days: None,
                    level,
                    group: None,
                    topic: None,
                })
                .expect("bench remember");
        });
    }

    // warm one recall so first-call embedder init doesn't skew the samples
    let _ = engine.recall(RecallInput {
        user: USER.into(),
        query: "warmup query".into(),
        project_id: None,
        k: None,
        min_similarity: None,
        write_allocate: None,
    });

    let mut recall_hit = Scenario::new("recall-hit");
    for (project, query) in &corpus.queries_hit {
        let project = project.clone();
        let query = query.clone();
        recall_hit.time(|| {
            let _ = engine
                .recall(RecallInput {
                    user: USER.into(),
                    query: query.clone(),
                    project_id: Some(project.clone()),
                    k: None,
                    min_similarity: None,
                    write_allocate: None,
                })
                .expect("bench recall");
        });
    }

    let mut recall_miss = Scenario::new("recall-miss");
    for query in &corpus.queries_miss {
        let query = query.clone();
        recall_miss.time(|| {
            let _ = engine
                .recall(RecallInput {
                    user: USER.into(),
                    query: query.clone(),
                    project_id: None,
                    k: None,
                    min_similarity: None,
                    write_allocate: None,
                })
                .expect("bench recall");
        });
    }

    let mut params = Scenario::new("params");
    for (id, _) in &corpus.projects {
        let id = id.clone();
        params.time(|| {
            let _ = engine
                .adjusted_parameters(USER, Some(&id))
                .expect("bench params");
        });
    }

    let consolidate_start = Instant::now();
    let report = engine.consolidate(USER).expect("bench consolidate");
    let consolidate_total = consolidate_start.elapsed();

    if !cfg.json {
        println!(
            "tiered-memory bench · embedder loaded in {} · {} projects · {} memories · store: {}",
            fmt_ms(load),
            corpus.projects.len(),
            memory_count,
            store_dir.display()
        );
        if cfg!(debug_assertions) {
            println!(
                "⚠ debug build — numbers are 10-50× slower than release and not comparable; use a release build (cargo build --release)"
            );
        }
        println!(
            "{:<14} {:>8} {:>10} {:>10} {:>12}",
            "scenario", "ops", "mean", "p95", "ops/s"
        );
        for s in [&register, &remember, &recall_hit, &recall_miss, &params] {
            println!(
                "{:<14} {:>8} {:>10} {:>10} {:>12.0}",
                s.name,
                s.samples.len(),
                fmt_ms(s.mean()),
                fmt_ms(s.percentile(95)),
                s.ops_per_s()
            );
        }
        println!(
            "{:<14} {:>8} {:>10}   (lifted {}, merged {})",
            "consolidate",
            1,
            fmt_ms(consolidate_total),
            report.traits_lifted,
            report.merged
        );
        println!(
            "\ncompare environments: TM_EMBEDDER=hashing|local|http · TM_DATA_DIR on tmpfs vs disk · rerun and diff"
        );
    } else {
        let results: Vec<serde_json::Value> =
            [&register, &remember, &recall_hit, &recall_miss, &params]
                .iter()
                .map(|s| {
                    serde_json::json!({
                        "scenario": s.name,
                        "ops": s.samples.len(),
                        "mean_ms": (s.mean_ms() * 1000.0).round() / 1000.0,
                        "p95_ms": s.percentile(95).as_secs_f64() * 1000.0,
                        "ops_per_s": s.ops_per_s(),
                    })
                })
                .collect();
        let out = serde_json::json!({
            "embedder_load_ms": load.as_secs_f64() * 1000.0,
            "projects": corpus.projects.len(),
            "memories": memory_count,
            "consolidate_ms": consolidate_total.as_secs_f64() * 1000.0,
            "traits_lifted": report.traits_lifted,
            "results": results,
        });
        println!("{out}");
    }

    if cfg.store.is_none() && !cfg.keep {
        let _ = std::fs::remove_dir_all(&store_dir);
    } else {
        println!("store kept: {}", store_dir.display());
    }
    Ok(())
}

const USER: &str = "bench";

/// `--embedder hashing|local|http`, falling back to `TM_EMBEDDER`/defaults —
/// the same knobs every other command reads, so environment comparisons stay
/// apples-to-apples. `http` reads `TM_LLM_BASE_URL`/`TM_LLM_API_KEY`/env like
/// the rest of the tool.
fn embedder_from_args(
    args: &[String],
) -> Result<std::sync::Arc<dyn tiered_memory::Embedder>, String> {
    let from_env = EmbedderConfig::from_env().map_err(|e| e.to_string())?;
    match arg_value(args, "--embedder").as_deref() {
        None => from_env.build().map_err(|e| e.to_string()),
        Some("hashing") => EmbedderConfig::Hashing { dims: 512 }
            .build()
            .map_err(|e| e.to_string()),
        Some("local") => {
            #[cfg(feature = "local")]
            {
                EmbedderConfig::Local {
                    model: "minilm".to_string(),
                    dir: None,
                    cache_dir: None,
                }
                .build()
                .map_err(|e| e.to_string())
            }
            #[cfg(not(feature = "local"))]
            {
                Err("this binary was built without the `local` feature — rebuild with `cargo install --path . --features local`".into())
            }
        }
        Some("http") => {
            #[cfg(feature = "http")]
            {
                // reuse whatever TM_LLM_* / credentials the tool already resolves
                let config = tiered_memory::LlmConfig::resolve(None, &crate::data_root())
                    .map_err(|e| e.to_string())?
                    .ok_or("no LLM credentials for the http embedder — run `tiered-memory credentials`")?;
                EmbedderConfig::Http {
                    url: format!("{}/embeddings", config.base_url.trim_end_matches('/')),
                    api_key: config.api_key.clone(),
                    model: Some(config.model.clone()),
                    dims: None,
                }
                .build()
                .map_err(|e| e.to_string())
            }
            #[cfg(not(feature = "http"))]
            {
                Err(
                    "this binary was built without the `http` feature — rebuild with `--features http`, or benchmark the http embedder via TM_EMBEDDER=http with an http-enabled build".into(),
                )
            }
        }
        Some(other) => Err(format!(
            "unknown --embedder `{other}` (hashing | local | http)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corpus_is_deterministic_and_structured() {
        let a = Corpus::generate(8, 6);
        let b = Corpus::generate(8, 6);
        assert_eq!(a.projects, b.projects, "same seed → same corpus");
        assert_eq!(a.memories, b.memories);
        // every project registered, each with per_project param-carrying lines
        assert_eq!(a.projects.len(), 8);
        let per_project_l1 = a
            .memories
            .iter()
            .filter(|(p, _, lvl, d)| *lvl == Level::L1 && p == &a.projects[0].0 && d.is_none())
            .count();
        assert_eq!(
            per_project_l1, 5,
            "5 plain L1 lines per project (6 minus param)"
        );
        // global traits exist with no project
        assert!(a
            .memories
            .iter()
            .any(|(p, _, lvl, _)| p.is_empty() && *lvl == Level::L3));
        // miss queries never share corpus vocabulary
        assert!(a.queries_miss.iter().all(|q| !q.contains("rust")));
    }

    #[test]
    fn percentiles_and_rates_are_sane() {
        let mut s = Scenario::new("t");
        for ms in [10, 20, 30, 40, 100] {
            s.time(|| std::thread::sleep(Duration::from_millis(ms)));
        }
        assert_eq!(s.samples.len(), 5);
        let p95 = s.percentile(95).as_millis();
        assert!(p95 >= 95, "p95 should land on the 100ms sample, got {p95}");
        let p50 = s.percentile(50).as_millis();
        assert_eq!(p50, 30, "p50 is the median sample");
        let rate = s.ops_per_s();
        assert!(
            rate > 5.0 && rate < 80.0,
            "5 ops in ~200ms → 20-30 ops/s, got {rate}"
        );
    }
}
