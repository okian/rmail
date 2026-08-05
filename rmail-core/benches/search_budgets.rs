//! Criterion harness for the search-path latency budgets in `prd.md`.
//!
//! # What is and isn't wired here
//!
//! `prd.md` names three headline budgets for the finished ranking pipeline
//! (also tracked in `tasks.md` task 86):
//!
//!   - first streamed hit visible:              < 30 ms
//!   - full ranked result (no Claude rerank):    < 150 ms
//!   - fuzzy finder keystroke -> first batch:    < 16 ms
//!
//! None of the three are benchable *as such* yet. "First streamed hit" and
//! "full ranked result" describe the finished multi-stage pipeline —
//! candidate generation across several retrievers, RRF fusion, feature
//! extraction, an L1 ranker, an L2 cross-encoder rerank, and a streaming
//! transport that flushes best-first — and that pipeline is tasks 25-33,
//! being built in parallel with this one and not merged yet. "Fuzzy finder
//! first batch" is task 59's `FinderStore`, which doesn't exist in
//! `rmail-core` at all today. Writing a `bench_function` against either would
//! mean benchmarking either nothing (a stub) or something that isn't the
//! thing the budget describes, which is worse than not having the benchmark:
//! a green run would look like proof of a budget nothing has actually met.
//!
//! What *does* exist and *is* wired below: [`rmail_core::index::FtsIndex`],
//! the field-weighted BM25 lexical retriever (task 21). It is one candidate
//! generator among the several the finished pipeline will fan out to in
//! parallel, so `prd.md`'s stage budget for that step —
//! "Candidate generation (all retrievers, parallel) < 25 ms" — is the
//! closest real, currently-measurable proxy: today lexical BM25 *is* the
//! entire candidate-generation stage, since it is the only retriever that
//! exists. [`lexical_candidate_generation`] benchmarks it over a synthetic
//! mailbox-shaped corpus and [`assert_lexical_budget`] turns that into a
//! pass/fail tripwire, both for a common two-term phrase (a large candidate
//! set to fuse and rank) and a rare single term (a small one).
//!
//! # Adding the real budgets later (the drop-in this task promises)
//!
//! When the search pipeline (tasks 25-33) lands a single ranked-search entry
//! point, add `bench_full_ranked_search`/`bench_first_streamed_hit` functions
//! here that call it instead of `FtsIndex::search` directly, reusing
//! [`seed_corpus`] for the fixture, and assert against
//! `Duration::from_millis(150)`/`Duration::from_millis(30)` per `prd.md`.
//! When the fuzzy finder (task 59) lands its `FinderStore`, add
//! `bench_fuzzy_first_batch` the same way against
//! `Duration::from_millis(16)`. Both should follow the same shape as
//! [`assert_lexical_budget`]: many samples, assert on the median, and keep
//! the safety factor this file uses (see [`BUDGET_SAFETY_FACTOR`]) — a
//! `cargo bench` invocation runs on whatever machine happens to call it, not
//! a dedicated idle benchmarking host, and this repo's own build cache is
//! shared across several concurrently-building agents.
//!
//! # Why criterion *and* a hand-rolled assertion
//!
//! Criterion's `bench_function` reports statistics (mean, median, outliers,
//! regressions against the last `cargo bench` run) for a human to read; it
//! does not fail the process on a threshold by itself. This file needs both:
//! criterion's reporting for `cargo bench`'s regular output, and a real
//! pass/fail signal for "did lexical retrieval blow its budget" that a CI
//! script or a developer can act on. Hence [`assert_lexical_budget`] runs its
//! own timing loop over the same fixture and operation and fails loudly if
//! the tripwire trips.
//!
//! "Fails loudly" here means propagating a `Result` with `?` up to a
//! hand-rolled [`main`] (see its own doc for why this file does not use the
//! `criterion_main!`/`criterion_group!` macros), never a literal
//! `panic!()`/`.unwrap()`/`.expect()` call: `[workspace.lints.clippy]` denies
//! `unwrap_used`/`expect_used`/`panic` for every target in this package,
//! including a `[[bench]]` one — there is no `#[cfg(test)]` carve-out for
//! benches the way there is for `#[test]` functions, so those lints apply
//! here exactly as they would in library code (verified while developing
//! this file: a stray `panic!()` was rejected by `cargo clippy --all-targets
//! --all-features -- -D warnings`, same as anywhere else in the workspace).
//! [`fts_search`] is the one exception, and says why in its own doc: it runs
//! inside a criterion-owned closure whose signature criterion's own API
//! fixes, with no `Result`-shaped way out.
//!
//! This target only needs to *compile* in CI (`cargo bench -p rmail-core
//! --no-run`, per this task's verify line) — actually running it, where the
//! assertions execute, is a `cargo bench` a developer invokes locally. A
//! shared, variably-loaded CI runner is exactly the kind of host the budget
//! safety factor above exists to tolerate, but "exists to tolerate load" and
//! "should be a required CI gate" are different claims, and this task does
//! not make the second one.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use criterion::Criterion;
use rmail_core::config::Bm25Weights;
use rmail_core::index::FtsIndex;
use rmail_core::repo;
use rmail_core::storage::Database;

/// Messages in the synthetic mailbox the benchmarks search over.
///
/// Large enough that BM25's IDF/length-normalization math has a real
/// distribution to work over (not the degenerate few-document case the unit
/// tests use), small enough that seeding it — which runs once per `cargo
/// bench` invocation, not per sample — stays in the low single-digit seconds
/// even on a loaded machine.
const CORPUS_SIZE: usize = 4_000;

/// prd.md's stage budget for "Candidate generation (all retrievers,
/// parallel)" — see the module doc for why this, and not the headline
/// first-hit/full-ranked numbers, is what lexical retrieval alone can
/// honestly be measured against today.
const CANDIDATE_GENERATION_BUDGET: Duration = Duration::from_millis(25);

/// How much slack the tripwire gives the budget above before failing.
///
/// Not a claim that 8x the PRD number is an acceptable production latency —
/// it very much is not. It is a concession that this file's assertion runs
/// wherever `cargo bench` is invoked, including this repo's own dev loop
/// where several agents build concurrently against one shared target
/// directory (see `.claude/BUILD_BRIEF.md`), and the question worth a hard
/// failure on a shared box is "did this regress by an order of magnitude,"
/// not "is this machine, right now, exactly as fast as idle M-series
/// hardware." A real regression trips this by miles; scheduler noise does
/// not.
const BUDGET_SAFETY_FACTOR: u32 = 8;

/// Samples taken per budget assertion. The tripwire compares the *median*
/// against budget specifically so that one unlucky sample (a GC-style pause,
/// a sibling process grabbing a core) cannot fail a build that is otherwise
/// fine — a real regression shifts the whole distribution, not one point in it.
const BUDGET_SAMPLES: usize = 50;

/// A deterministic, dependency-free PRNG (SplitMix64).
///
/// A benchmark fixture needs reproducible content across runs — so the
/// candidate sets and therefore the timings are comparable run to run — not
/// cryptographic randomness, and pulling in `rand` for that would be a
/// workspace dependency this task does not own (see `.claude/BUILD_BRIEF.md`
/// on keeping shared-file diffs minimal).
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A uniform index into `0..n`. `n` is always a small, non-zero,
    /// compile-time-known vocabulary length in this file, so the modulo bias
    /// from `u64::MAX` not being a multiple of `n` is not worth a rejection
    /// loop to remove.
    fn index(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }

    /// An inclusive range, for word counts.
    fn range(&mut self, lo: usize, hi: usize) -> usize {
        lo + self.index(hi - lo + 1)
    }
}

/// A business-email-flavored vocabulary with no occurrence of either
/// benchmark query's terms — `word()` below draws from this for the filler
/// text every message gets, and the query terms are injected separately and
/// deliberately (see [`seed_corpus`]) so the match rate is a controlled,
/// documented number rather than whatever this list's incidental word
/// frequency happens to produce.
const VOCAB: &[&str] = &[
    "meeting",
    "budget",
    "project",
    "deadline",
    "team",
    "update",
    "schedule",
    "client",
    "proposal",
    "contract",
    "payment",
    "vendor",
    "report",
    "attached",
    "please",
    "review",
    "approve",
    "thanks",
    "regards",
    "following",
    "discussed",
    "action",
    "items",
    "next",
    "steps",
    "call",
    "tomorrow",
    "week",
    "sales",
    "marketing",
    "engineering",
    "release",
    "notes",
    "summary",
    "draft",
    "final",
    "version",
    "attachment",
    "document",
    "spreadsheet",
    "presentation",
    "slides",
    "agenda",
    "minutes",
    "decision",
    "status",
    "issue",
    "bug",
    "fix",
    "feature",
    "request",
    "access",
    "account",
    "login",
    "password",
    "reset",
    "security",
    "policy",
    "compliance",
    "audit",
    "legal",
    "onboarding",
    "training",
    "conference",
    "travel",
    "expense",
    "reimbursement",
    "receipt",
    "tax",
    "shipment",
    "delivery",
    "tracking",
    "order",
    "confirmation",
    "cancel",
    "refund",
    "subscription",
    "renewal",
    "license",
    "server",
    "database",
    "deployment",
    "production",
    "staging",
    "outage",
    "monitoring",
    "alert",
    "dashboard",
    "metrics",
    "performance",
    "latency",
    "capacity",
    "backup",
    "recovery",
    "plan",
    "roadmap",
    "milestone",
    "sprint",
    "standup",
    "kickoff",
    "launch",
    "announcement",
    "newsletter",
    "customer",
    "feedback",
    "survey",
    "support",
    "ticket",
    "escalation",
    "priority",
    "urgent",
    "reminder",
    "followup",
    "reschedule",
    "cancelled",
    "confirmed",
    "pending",
    "approved",
    "rejected",
    "signed",
    "executed",
    "amendment",
    "termination",
];

/// Distinctive, single-occurrence-shaped term seeded into a small slice of
/// the corpus, for a benchmark over a narrow candidate set.
const RARE_TERM: &str = "reconciliation";

/// Two ordinary words seeded together into a larger slice of the corpus, for
/// a benchmark over a wide candidate set and a phrase-adjacency query.
const COMMON_PHRASE: &str = "quarterly report";

/// Owns the temp database file (and its `-wal`/`-shm` siblings) for the
/// lifetime of one `cargo bench` invocation, mirroring the pattern in
/// `rmail-core/src/storage/tests.rs` — WAL mode requires a real file, so
/// `:memory:` cannot stand in here.
struct Fixture {
    fts: FtsIndex,
    path: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.path.display())));
        }
    }
}

/// Build a `size`-message mailbox with two controlled, documented match
/// rates: [`RARE_TERM`] lands in roughly 1 message in 40 (~2.5%), and
/// [`COMMON_PHRASE`] lands in roughly 1 in 8 (~12.5%) — enough of a spread
/// that the two benchmark queries below exercise meaningfully different
/// candidate-set sizes, not the same query twice under different names.
///
/// Bypasses [`rmail_core::index::extract_message`] and writes directly into
/// `index_content`, the same shortcut `rmail-core/src/index/entities/
/// tests.rs` takes for its own large-fixture test: extraction's own text
/// normalization is not what this file measures, and running the real
/// extraction pipeline message-by-message would make seeding — which is
/// setup, not the timed operation — the dominant cost of running this
/// benchmark at all. [`FtsIndex::index_message`] itself, the code actually
/// under measurement, is not bypassed.
async fn seed_corpus(size: usize) -> Result<Fixture, rmail_core::Error> {
    let pid = std::process::id();
    let path = std::env::temp_dir().join(format!("rmail-bench-search-{pid}.db"));
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", path.display())));
    }
    let db = Database::open(&path)?;

    let (account_id, mailbox_id) = db
        .write(|c| {
            let account_id = repo::insert_account(
                c,
                &repo::NewAccount {
                    name: "Bench".to_owned(),
                    ..Default::default()
                },
            )?;
            let mailbox_id = repo::insert_mailbox(
                c,
                &repo::NewMailbox {
                    account_id,
                    name: "INBOX".to_owned(),
                    ..Default::default()
                },
            )?;
            Ok((account_id, mailbox_id))
        })
        .await?;

    // One transaction for every message row plus its two `index_content`
    // rows: seeding is setup, and thousands of individually-awaited writer
    // round trips would make it the dominant cost of running this file.
    let message_ids: Vec<i64> = db
        .write(move |c| {
            let tx = c.transaction()?;
            let mut ids = Vec::with_capacity(size);
            let mut rng = Rng(0x5EED_5EED_5EED_5EED);
            for i in 0..size {
                let subject = random_words(&mut rng, 3, 7);
                let mut body = random_words(&mut rng, 60, 150);
                if i % 40 == 0 {
                    body.push(' ');
                    body.push_str(RARE_TERM);
                }
                if i % 8 == 0 {
                    body.push(' ');
                    body.push_str(COMMON_PHRASE);
                }

                let message_id = repo::insert_message(
                    &tx,
                    &repo::NewMessage {
                        account_id,
                        mailbox_id,
                        uid: i as i64 + 1,
                        uidvalidity: 1,
                        subject: Some(subject.clone()),
                        body_text: Some(body.clone()),
                        ..Default::default()
                    },
                )?;

                tx.execute(
                    "INSERT INTO index_content
                         (message_id, part, text, chars, content_hash, extractor)
                     VALUES (?1, 'subject', ?2, ?3, X'00', 'bench')",
                    rusqlite::params![message_id, subject.clone(), subject.len() as i64],
                )?;
                tx.execute(
                    "INSERT INTO index_content
                         (message_id, part, text, chars, content_hash, extractor)
                     VALUES (?1, 'body', ?2, ?3, X'00', 'bench')",
                    rusqlite::params![message_id, body.clone(), body.len() as i64],
                )?;

                ids.push(message_id);
            }
            tx.commit()?;
            Ok(ids)
        })
        .await?;

    let fts = FtsIndex::new(db, Bm25Weights::default());
    for message_id in message_ids {
        fts.index_message(message_id).await?;
    }

    Ok(Fixture { fts, path })
}

fn random_words(rng: &mut Rng, min: usize, max: usize) -> String {
    let n = rng.range(min, max);
    (0..n)
        .map(|_| VOCAB[rng.index(VOCAB.len())])
        .collect::<Vec<_>>()
        .join(" ")
}

/// Attach context to a `Result`'s error without the denied `.unwrap()`/
/// `.expect()` (see the module doc "Why criterion and a hand-rolled
/// assertion"). Used with `?` at every call site below that can propagate a
/// real `Result` up to [`main`] — which is every fallible step in this file
/// *except* [`fts_search`], where criterion's own `Bencher::iter` API gives
/// no `Result`-shaped channel to propagate through (see that function's doc).
fn ctx<T, E: std::fmt::Display>(result: Result<T, E>, context: &str) -> Result<T, String> {
    result.map_err(|e| format!("{context}: {e}"))
}

/// Build the tokio runtime the async fixture setup and search calls run on.
fn build_runtime() -> Result<tokio::runtime::Runtime, String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build();
    ctx(
        runtime,
        "failed to build the tokio runtime for search_budgets",
    )
}

fn build_fixture(rt: &tokio::runtime::Runtime) -> Result<Fixture, String> {
    ctx(
        rt.block_on(seed_corpus(CORPUS_SIZE)),
        "failed to seed the search_budgets fixture",
    )
}

/// Take [`BUDGET_SAMPLES`] direct timings of `fts.search(query, ..)` and
/// check the median against `budget * BUDGET_SAFETY_FACTOR`.
///
/// Separate from the `criterion::bench_function` calls in
/// [`lexical_candidate_generation`] on purpose: criterion's own sampling
/// exists for statistical reporting across `cargo bench` runs, not as a
/// pass/fail gate a caller can act on, so this file needs its own
/// measurement loop for that half of the job. Both loops exercise the exact
/// same [`FtsIndex::search`] call over the exact same fixture.
///
/// # Errors
///
/// A search failing outright is [`ctx`]'d and propagated with `?`, same as
/// fixture setup. A search that *succeeds* but violates an expectation
/// (matched nothing, or blew the latency budget) is a different kind of
/// failure — a computed property of the results, not a `Result` to unwrap —
/// and is returned as its own descriptive `Err` rather than forced through
/// `ctx`, which exists for the former case.
fn assert_lexical_budget(
    rt: &tokio::runtime::Runtime,
    fts: &FtsIndex,
    label: &str,
    query: &str,
) -> Result<(), String> {
    // One untimed call first: the first query against a freshly built FTS5
    // index pays for page-cache warmup that every subsequent query does not,
    // and that one-time cost is not what "candidate generation" budgets.
    let _ = rt.block_on(fts.search(query, 200));

    let mut samples = Vec::with_capacity(BUDGET_SAMPLES);
    for _ in 0..BUDGET_SAMPLES {
        let started = Instant::now();
        let result = rt.block_on(fts.search(query, 200));
        let elapsed = started.elapsed();
        let hits = ctx(
            result,
            &format!("lexical search failed during the {label} budget check"),
        )?;
        samples.push((elapsed, hits.len()));
    }
    samples.sort_by_key(|(d, _)| *d);
    let (median, hit_count) = samples[samples.len() / 2];
    let budget = CANDIDATE_GENERATION_BUDGET * BUDGET_SAFETY_FACTOR;

    // A search that always returns zero hits would also always be "fast" —
    // the fastest possible query is one that touches nothing. Without this,
    // a regression that silently broke seeding or indexing (an empty FTS
    // table, a query that no longer matches anything) would read as an
    // *improvement* in the budget check below rather than the failure it is.
    if hit_count == 0 {
        return Err(format!(
            "{label}: {query:?} matched 0 of {CORPUS_SIZE} messages — seed_corpus or \
             FtsIndex::index_message is broken, not fast"
        ));
    }
    if median > budget {
        return Err(format!(
            "{label}: median lexical search latency {median:?} exceeded the budget \
             {budget:?} ({CANDIDATE_GENERATION_BUDGET:?} x {BUDGET_SAFETY_FACTOR} safety factor) \
             over {CORPUS_SIZE} messages, {hit_count} hits for {query:?}"
        ));
    }
    Ok(())
}

fn lexical_candidate_generation(c: &mut Criterion) -> Result<(), String> {
    let rt = build_runtime()?;
    let fixture = build_fixture(&rt)?;

    let mut group = c.benchmark_group("lexical_candidate_generation");
    group.bench_function("common_phrase", |b| {
        b.to_async(&rt)
            .iter(|| async { fts_search(&fixture.fts, COMMON_PHRASE).await });
    });
    group.bench_function("rare_term", |b| {
        b.to_async(&rt)
            .iter(|| async { fts_search(&fixture.fts, RARE_TERM).await });
    });
    group.finish();

    assert_lexical_budget(&rt, &fixture.fts, "common_phrase", COMMON_PHRASE)?;
    assert_lexical_budget(&rt, &fixture.fts, "rare_term", RARE_TERM)?;
    Ok(())
}

/// Run one search inside a criterion `iter` closure. `criterion::black_box`
/// on the query keeps the compiler from const-folding a call whose argument
/// never changes across samples; the result is returned (rather than
/// discarded) so criterion also cannot conclude the call is dead and elide
/// it.
///
/// The one place in this file that cannot propagate a `Result`: criterion's
/// `Bencher::to_async(..).iter()` calls this closure directly and expects a
/// plain value back, thousands of times per benchmark, with no `Result`-
/// shaped channel to abort the run through — that shape is fixed by
/// criterion's own API, not a choice made here. `unreachable!` is what's left
/// once `.unwrap()`/`.expect()`/`panic!()` are off the table (see the module
/// doc). It is a stretch of the macro's literal meaning — a search failing
/// here is not *impossible*, just unhandleable through this call shape — but
/// it is the narrowest such stretch available: the identical query, against
/// the identical fixture and connection, already succeeded once in
/// [`assert_lexical_budget`]'s untimed warmup call moments earlier, so a
/// failure appearing only now would mean the connection or the database file
/// itself broke mid-run — a fault in the process's environment, not a
/// foreseeable outcome of this specific call worth its own recovery path.
async fn fts_search(fts: &FtsIndex, query: &str) -> usize {
    let result = fts
        .search(criterion::black_box(query), criterion::black_box(200))
        .await;
    match result {
        Ok(hits) => hits.len(),
        Err(e) => unreachable!("lexical search failed during the criterion sample loop: {e}"),
    }
}

/// Hand-rolled in place of the `criterion_group!`/`criterion_main!` macros
/// (see their expansion in `criterion::macros`) specifically so
/// [`lexical_candidate_generation`]'s fallible setup can return a `Result`
/// and propagate with `?` — the macros fix that function's signature to
/// `fn(&mut Criterion)`, which has no room for one. A `Result`-returning
/// `main` reports its `Err` and exits non-zero exactly like a panic would,
/// but through an ordinary early return: every local still in scope at that
/// point (in particular, [`Fixture`], borrowed for the whole benchmark
/// group) is dropped exactly as it would be on any other return, no
/// unwinding required to get there.
fn main() -> Result<(), String> {
    let mut criterion = Criterion::default().configure_from_args();
    lexical_candidate_generation(&mut criterion)?;
    Criterion::default().configure_from_args().final_summary();
    Ok(())
}
