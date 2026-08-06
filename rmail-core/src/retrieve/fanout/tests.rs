//! What the fan-out owes the pipeline: every enabled source's candidates
//! come back tagged with their own `Source`, a source disabled by config
//! contributes nothing without being called at all, a source that genuinely
//! fails at runtime degrades to nothing without taking any other source down
//! with it, an already-superseded query returns quickly with nothing rather
//! than waiting out every retriever, and — the substance of this task —
//! running every source is measurably faster than running them one after
//! another, proving `tokio::join!` actually overlaps their work instead of
//! serializing it.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use super::*;
use crate::config::{Bm25Weights, ExpansionConfig, IndexSemanticConfig};
use crate::embed::hash::HashEmbedder;
use crate::index::{extract_entities, FtsIndex};
use crate::query::QueryPlanner;
use crate::repo;

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct Fixture {
    db: Database,
    fts: FtsIndex,
    semantic: SemanticIndex,
    planner: QueryPlanner,
    account_id: i64,
    mailbox_id: i64,
    next_uid: std::cell::Cell<i64>,
    path: PathBuf,
}

impl Fixture {
    async fn open() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-retrieve-fanout-{pid}-{n}.db"));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", path.display())));
        }
        let db = Database::open(&path).unwrap();
        let (account_id, mailbox_id) = db
            .write(|c| {
                let account_id = repo::insert_account(
                    c,
                    &repo::NewAccount {
                        name: "Personal".to_owned(),
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
            .await
            .unwrap();
        let embedder = std::sync::Arc::new(HashEmbedder::new(crate::index::semantic::VECTOR_DIM));
        let semantic = SemanticIndex::new(
            db.clone(),
            embedder.clone(),
            &IndexSemanticConfig {
                chunk_tokens: 32,
                chunk_overlap: 4,
                ..IndexSemanticConfig::default()
            },
        );
        let fts = FtsIndex::new(db.clone(), Bm25Weights::default());
        let planner = QueryPlanner::new(db.clone(), embedder, ExpansionConfig::default());
        Self {
            fts,
            semantic,
            planner,
            db,
            account_id,
            mailbox_id,
            next_uid: std::cell::Cell::new(1),
            path,
        }
    }

    /// Insert a message fully indexed on every axis this task's retrievers
    /// read: lexical, semantic, and entity.
    async fn message(&self, subject: &str, body: &str, from_addr: &str) -> i64 {
        let uid = self.next_uid.get();
        self.next_uid.set(uid + 1);
        let (account_id, mailbox_id) = (self.account_id, self.mailbox_id);
        let (subject_owned, body_owned, from_owned) =
            (subject.to_owned(), body.to_owned(), from_addr.to_owned());
        let message_id = self
            .db
            .write(move |c| {
                let id = repo::insert_message(
                    c,
                    &repo::NewMessage {
                        account_id,
                        mailbox_id,
                        uid,
                        uidvalidity: 1,
                        subject: Some(subject_owned.clone()),
                        from_addr: Some(from_owned),
                        date: Some(1_700_000_000 + uid),
                        ..Default::default()
                    },
                )?;
                for (part, text) in [("subject", subject_owned.as_str()), ("body", &body_owned)] {
                    c.execute(
                        "INSERT INTO index_content
                             (message_id, part, text, chars, content_hash, extractor)
                         VALUES (?1, ?2, ?3, ?4, X'00', 'test')",
                        rusqlite::params![id, part, text, text.len() as i64],
                    )?;
                }
                Ok(id)
            })
            .await
            .unwrap();
        self.fts.index_message(message_id).await.unwrap();
        self.semantic.index_message(message_id).await.unwrap();
        extract_entities(&self.db, message_id).await.unwrap();
        message_id
    }

    async fn plan(&self, raw: &str) -> QueryPlan {
        self.planner.plan(raw).await.unwrap()
    }

    fn fanout(&self, config: &RetrieversConfig) -> Fanout {
        Fanout::new(self.db.clone(), self.fts.clone(), &self.semantic, config)
    }

    fn no_cancel() -> CancellationToken {
        CancellationToken::new()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.path.display())));
        }
    }
}

fn all_enabled() -> RetrieversConfig {
    RetrieversConfig {
        dense: true,
        fuzzy: true,
        entity: true,
        structured: true,
        prefix: true,
        recency: true,
        recency_half_life_days: 30.0,
    }
}

#[tokio::test]
async fn multiple_sources_contribute_for_a_query_every_one_of_them_can_answer() {
    let fx = Fixture::open().await;
    fx.message(
        "Acme quarterly invoice",
        "please find the quarterly invoice attached, billing@acme.example",
        "billing@acme.example",
    )
    .await;

    let plan = fx.plan("invoice from:billing@acme.example").await;
    let fanout = fx.fanout(&all_enabled());
    let candidates = fanout.generate(&plan, 50, &Fixture::no_cancel()).await;

    let sources: std::collections::HashSet<Source> = candidates.iter().map(|c| c.source).collect();
    // Lexical (the word "invoice"), structured/recency (the `from:` filter
    // gates and every survivor is recency-ordered), and prefix (the same
    // word as a prefix match) all have something to say about this query;
    // asserting several rather than exactly one guards against a wiring bug
    // that silently dropped one source from the `join!`.
    assert!(
        sources.len() >= 3,
        "expected several sources to contribute, got {sources:?}"
    );
    assert!(sources.contains(&Source::Lexical));
    assert!(sources.contains(&Source::Structured));
}

#[tokio::test]
async fn a_source_disabled_by_config_is_never_called() {
    let fx = Fixture::open().await;
    fx.message(
        "budget report",
        "the quarterly budget report covers spending across every team",
        "alice@example.com",
    )
    .await;

    let plan = fx.plan("budget report").await;

    let mut disabled = all_enabled();
    disabled.dense = false;
    let candidates = fx
        .fanout(&disabled)
        .generate(&plan, 50, &Fixture::no_cancel())
        .await;
    assert!(
        !candidates.iter().any(|c| c.source == Source::Dense),
        "a config-disabled retriever must not contribute any candidates"
    );

    let enabled = fx
        .fanout(&all_enabled())
        .generate(&plan, 50, &Fixture::no_cancel())
        .await;
    assert!(
        enabled.iter().any(|c| c.source == Source::Dense),
        "sanity check: the same query does produce a dense candidate when enabled"
    );
}

#[tokio::test]
async fn a_source_that_fails_at_runtime_degrades_without_taking_the_others_down() {
    let fx = Fixture::open().await;
    fx.message(
        "budget report",
        "the quarterly budget report covers spending across every team",
        "alice@example.com",
    )
    .await;

    // A hand-built plan whose `raw` contains an embedded NUL byte: task 27's
    // lexical retriever genuinely errors on this (`fts::malformed_query`,
    // proven in `retrieve::lexical`'s own tests) once it re-derives a
    // `MATCH` string from it — a real failure this fan-out has to survive,
    // not a mock.
    let mut plan = fx.plan("budget").await;
    plan.raw = "foo\0bar".to_owned();

    let candidates = fx
        .fanout(&all_enabled())
        .generate(&plan, 50, &Fixture::no_cancel())
        .await;
    assert!(
        !candidates.iter().any(|c| c.source == Source::Lexical),
        "the failing source must not appear"
    );
    assert!(
        candidates
            .iter()
            .any(|c| c.source == Source::Structured || c.source == Source::Recency),
        "other sources must still have run and returned candidates: {candidates:?}"
    );
}

#[tokio::test]
async fn an_already_cancelled_token_returns_quickly_with_nothing() {
    let fx = Fixture::open().await;
    for i in 0..5 {
        fx.message(
            &format!("budget report {i}"),
            "the quarterly budget report covers spending",
            "alice@example.com",
        )
        .await;
    }

    let plan = fx.plan("budget report from:alice").await;
    let cancel = CancellationToken::new();
    cancel.cancel();

    let started = Instant::now();
    let candidates = fx.fanout(&all_enabled()).generate(&plan, 50, &cancel).await;
    // Every source, including lexical, reads `cancel` and must contribute
    // nothing once it has already fired.
    for source in [
        Source::Lexical,
        Source::Dense,
        Source::Fuzzy,
        Source::Entity,
        Source::Structured,
        Source::Prefix,
        Source::Recency,
    ] {
        assert!(
            !candidates.iter().any(|c| c.source == source),
            "{source:?} must contribute nothing once its token is already cancelled"
        );
    }
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "a superseded fan-out must not hang waiting on cancelled sources"
    );
}

#[tokio::test]
async fn running_every_source_concurrently_beats_running_them_one_after_another() {
    // A wall-clock comparison only has room to show overlap when there is
    // more than one core for the retrievers to actually run on — below
    // that, `tokio::join!` still polls every branch concurrently (the
    // property under test), but a single core has nowhere to *run* two of
    // them at once, so "concurrent" degenerates toward "sequential plus
    // scheduling overhead" for reasons that have nothing to do with a
    // regression in this code. Skipping (not failing) on such a runner is
    // more honest than a threshold tuned to pass anyway.
    let cores = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    if cores < 3 {
        eprintln!(
            "skipping: only {cores} core(s) available, not enough to make a wall-clock \
             concurrency comparison meaningful"
        );
        return;
    }

    let fx = Fixture::open().await;
    for i in 0..120 {
        fx.message(
            &format!("Acme invoice {i} quarterly statement"),
            &format!(
                "please review invoice number INV-{i:05} for the quarterly statement, \
                 order ORD-{i:05} ships separately, billing@acme{i}.example"
            ),
            &format!("billing@acme{i}.example"),
        )
        .await;
    }

    let plan = fx.plan("quarterly invoice statement from:billing").await;
    let fanout = fx.fanout(&all_enabled());
    let cancel = Fixture::no_cancel();

    // Warm-up, deliberately excluded from both measurements below. Tokio's
    // blocking-thread pool and `Database`'s read-connection pool both grow to
    // their steady-state size lazily, on first use — new OS threads, new
    // pooled connections — and running seven retrievers *concurrently* for
    // the first time needs up to seven of each at once, a one-time cost far
    // larger than the millisecond-scale work this test actually wants to
    // measure. Priming with a concurrent call (the shape with the largest
    // simultaneous demand) here means neither timing below is contaminated
    // by it, so the comparison reflects scheduling, not cold-start cost.
    for _ in 0..3 {
        let _ = fanout.generate(&plan, 50, &cancel).await;
    }

    // Sequential baseline: every enabled retriever, one after another,
    // against the identical dataset and plan the concurrent run below uses —
    // the fairest possible comparison, since it is the same work either way,
    // only the scheduling differs. Summed over several iterations (rather
    // than judged on one) to average out scheduler noise on a loaded box.
    // The *minimum* across trials, not the sum. Summing accumulates every
    // scheduler hiccup either side happened to catch, so on a machine running
    // other work — several build agents, say — noise can swamp the overlap
    // this is trying to observe and the comparison flips for reasons unrelated
    // to the code. A minimum converges on the uncontended cost instead: load
    // can only ever make a trial slower, so the fastest of several is the one
    // least contaminated by it, and it is the honest estimate of what the
    // scheduling actually buys.
    let mut sequential = Duration::MAX;
    for _ in 0..7 {
        let start = Instant::now();
        let _ = fanout.run_lexical(&plan, 50, &cancel).await;
        let _ = fanout.run_dense(&plan, 50, &cancel).await;
        let _ = fanout.run_fuzzy(&plan, 50, &cancel).await;
        let _ = fanout.run_entity(&plan, 50, &cancel).await;
        let _ = fanout.run_structured(&plan, 50, &cancel).await;
        let _ = fanout.run_prefix(&plan, 50, &cancel).await;
        let _ = fanout.run_recency(&plan, 50, &cancel).await;
        sequential = sequential.min(start.elapsed());
    }

    let mut concurrent = Duration::MAX;
    let mut candidates = Vec::new();
    for _ in 0..7 {
        let start = Instant::now();
        candidates = fanout.generate(&plan, 50, &cancel).await;
        concurrent = concurrent.min(start.elapsed());
    }

    assert!(!candidates.is_empty());
    // Strictly less, not multiplied by a margin: a margin tuned to pass
    // reliably on this box is not a portable claim about a CI runner's core
    // count or load, and "not slower than running the same seven calls
    // back-to-back" already refutes serialization on its own — if `join!`
    // were accidentally replaced with sequential `.await`s, `concurrent`
    // would be *at least* `sequential` (the same work, plus its own
    // overhead), never less.
    assert!(
        concurrent < sequential,
        "expected `tokio::join!` to overlap the retrievers' work: best-of-7 sequential took \
         {sequential:?}, best-of-7 concurrent took {concurrent:?} (concurrent should be less \
         than sequential, not merely equal to it — that would mean the sources ran one after \
         another instead of together)"
    );
}
