//! What the lexical retriever owes a ranker: BM25 candidates that actually
//! respect the query's hard filters, phrase and proximity semantics that
//! match what a user typed, and a `MATCH` builder that cannot be steered by
//! FTS5 syntax hiding in ordinary search text.

use std::cell::Cell;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use tokio_util::sync::CancellationToken;

use super::*;
use crate::config::Bm25Weights;
use crate::index::{extract_message, IndexQueue, QueueOptions, PRIORITY_NORMAL};
use crate::query::{self, Mode, Term};
use crate::repo;
use crate::ErrorReason;

/// A token that is never cancelled, for tests that only care about ranking
/// behavior — `retrieve::cancel`'s own tests are what prove the cancellation
/// contract itself.
fn no_cancel() -> CancellationToken {
    CancellationToken::new()
}

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct Fixture {
    db: Database,
    fts: FtsIndex,
    retriever: LexicalRetriever,
    queue: IndexQueue,
    account_id: i64,
    mailbox_id: i64,
    next_uid: Cell<i64>,
    path: PathBuf,
}

impl Fixture {
    async fn open() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-retrieve-lexical-{pid}-{n}.db"));
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
        let fts = FtsIndex::new(db.clone(), Bm25Weights::default());
        Self {
            retriever: LexicalRetriever::new(fts.clone(), db.clone()),
            fts,
            queue: IndexQueue::new(db.clone(), QueueOptions::default()),
            db,
            account_id,
            mailbox_id,
            next_uid: Cell::new(1),
            path,
        }
    }

    /// Store a message, extract it, and index it — the real pipeline.
    /// Honors an explicit `account_id`/`mailbox_id` on `new` (nonzero, since
    /// real ids start at 1) rather than always defaulting to the fixture's
    /// own account/mailbox, so scope-filter tests can place a message
    /// elsewhere.
    async fn index(&self, new: repo::NewMessage) -> i64 {
        let uid = self.next_uid.get();
        self.next_uid.set(uid + 1);
        let account_id = if new.account_id != 0 {
            new.account_id
        } else {
            self.account_id
        };
        let mailbox_id = if new.mailbox_id != 0 {
            new.mailbox_id
        } else {
            self.mailbox_id
        };
        let new = repo::NewMessage {
            account_id,
            mailbox_id,
            uid,
            uidvalidity: 1,
            ..new
        };
        let message_id = self
            .db
            .write(move |c| repo::insert_message(c, &new))
            .await
            .unwrap();
        extract_message(&self.db, &self.queue, message_id, PRIORITY_NORMAL)
            .await
            .unwrap();
        self.fts.index_message(message_id).await.unwrap();
        message_id
    }

    async fn flag(&self, message_id: i64, flag: &str) {
        let flag = flag.to_owned();
        self.db
            .write(move |c| repo::add_flag(c, message_id, &flag))
            .await
            .unwrap();
    }

    async fn attach(&self, message_id: i64, filename: &str) {
        let filename = filename.to_owned();
        self.db
            .write(move |c| {
                repo::insert_attachment(
                    c,
                    &repo::NewAttachment {
                        message_id,
                        filename: Some(filename),
                        ..Default::default()
                    },
                )
            })
            .await
            .unwrap();
    }

    async fn account(&self, name: &str) -> i64 {
        let name = name.to_owned();
        self.db
            .write(move |c| {
                repo::insert_account(
                    c,
                    &repo::NewAccount {
                        name,
                        ..Default::default()
                    },
                )
            })
            .await
            .unwrap()
    }

    async fn mailbox(&self, account_id: i64, name: &str) -> i64 {
        let name = name.to_owned();
        self.db
            .write(move |c| {
                repo::insert_mailbox(
                    c,
                    &repo::NewMailbox {
                        account_id,
                        name,
                        ..Default::default()
                    },
                )
            })
            .await
            .unwrap()
    }

    async fn thread(&self) -> i64 {
        let account_id = self.account_id;
        self.db
            .write(move |c| {
                repo::insert_thread(
                    c,
                    &repo::NewThread {
                        account_id,
                        ..Default::default()
                    },
                )
            })
            .await
            .unwrap()
    }

    async fn retrieve(&self, parsed: &query::ParsedQuery) -> Vec<Candidate> {
        self.retriever
            .retrieve(parsed, 100, &no_cancel())
            .await
            .unwrap()
    }

    /// Parse `raw` with the real operator parser and retrieve, returning just
    /// the message ids in rank order.
    async fn ids(&self, raw: &str) -> Vec<i64> {
        let parsed = query::parse(raw);
        self.retrieve(&parsed)
            .await
            .into_iter()
            .map(|c| c.message_id)
            .collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.path.display())));
        }
    }
}

/// Build a single bare `Term` with no negation/sigil — the shape most manual
/// `ParsedQuery`s in this file need.
fn bare_term(text: &str) -> Term {
    Term {
        text: text.to_owned(),
        negated: false,
        mode: Mode::Auto,
        looked_like_operator: false,
    }
}

// ---------------------------------------------------------------------------
// Top-N, score, and rank
// ---------------------------------------------------------------------------

#[tokio::test]
async fn candidates_carry_source_local_score_and_one_based_rank() {
    let fx = Fixture::open().await;
    let strong = fx
        .index(repo::NewMessage {
            subject: Some("budget".to_owned()),
            body_text: Some("budget".to_owned()),
            ..Default::default()
        })
        .await;
    let weak = fx
        .index(repo::NewMessage {
            body_text: Some("a passing mention of budget".to_owned()),
            ..Default::default()
        })
        .await;

    let hits = fx.retrieve(&query::parse("budget")).await;

    assert_eq!(hits.len(), 2);
    assert!(hits.iter().all(|c| c.source == Source::Lexical));
    assert_eq!(hits[0].message_id, strong);
    assert_eq!(hits[0].rank, 1);
    assert_eq!(hits[1].message_id, weak);
    assert_eq!(hits[1].rank, 2);
    assert!(hits[0].score > hits[1].score);
}

#[tokio::test]
async fn limit_is_clamped_like_fts_index_search() {
    let fx = Fixture::open().await;
    for n in 0..5 {
        fx.index(repo::NewMessage {
            subject: Some(format!("hedgehog {n}")),
            ..Default::default()
        })
        .await;
    }
    let parsed = query::parse("hedgehog");

    assert_eq!(
        fx.retriever
            .retrieve(&parsed, 2, &no_cancel())
            .await
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        fx.retriever
            .retrieve(&parsed, 0, &no_cancel())
            .await
            .unwrap()
            .len(),
        5,
        "zero means the server default"
    );
    assert_eq!(
        fx.retriever
            .retrieve(&parsed, -1, &no_cancel())
            .await
            .unwrap()
            .len(),
        5
    );
}

#[tokio::test]
async fn a_pure_filter_query_with_no_free_text_returns_nothing() {
    let fx = Fixture::open().await;
    fx.index(repo::NewMessage {
        from_addr: Some("alice@example.com".to_owned()),
        body_text: Some("invoice".to_owned()),
        ..Default::default()
    })
    .await;

    // Filters gate; they do not rank. With nothing to rank on, a BM25
    // retriever has nothing to return, even though a message exists that
    // would pass the filter.
    assert!(fx.ids("from:alice").await.is_empty());
}

// ---------------------------------------------------------------------------
// Phrase and proximity
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_quoted_phrase_requires_adjacency() {
    let fx = Fixture::open().await;
    let exact = fx
        .index(repo::NewMessage {
            body_text: Some("please review the quarterly report before friday".to_owned()),
            ..Default::default()
        })
        .await;
    fx.index(repo::NewMessage {
        body_text: Some("the report covers a quarterly cadence".to_owned()),
        ..Default::default()
    })
    .await;

    assert_eq!(fx.ids("\"quarterly report\"").await, vec![exact]);
    assert_eq!(
        fx.ids("quarterly report").await.len(),
        2,
        "the unquoted query still matches both"
    );
}

#[tokio::test]
async fn unquoted_terms_close_together_rank_above_the_same_terms_far_apart() {
    let fx = Fixture::open().await;
    let filler = (1..=12)
        .map(|n| format!("filler{n}"))
        .collect::<Vec<_>>()
        .join(" ");

    // Identical bags of words (the 2 query terms plus the same 12 distinct
    // filler words) in both messages, so raw BM25 — term frequency and
    // document length, no positional component — cannot tell them apart.
    // Only the proximity bonus can explain an order between them.
    let near = fx
        .index(repo::NewMessage {
            body_text: Some(format!("alpha beta {filler}")),
            ..Default::default()
        })
        .await;
    let far = fx
        .index(repo::NewMessage {
            body_text: Some(format!("alpha {filler} beta")),
            ..Default::default()
        })
        .await;

    let hits = fx.retrieve(&query::parse("alpha beta")).await;

    assert_eq!(hits.len(), 2, "the unquoted query still matches both");
    assert_eq!(hits[0].message_id, near, "the tighter match ranks first");
    assert_eq!(hits[1].message_id, far);
    assert!(
        hits[0].score > hits[1].score,
        "and outscores it: {} vs {}",
        hits[0].score,
        hits[1].score
    );
}

#[tokio::test]
async fn a_single_term_gets_no_proximity_probe() {
    // Nothing to be "near" with only one eligible term — this is really a
    // guard against a probe that accidentally runs (and errors, since
    // `NEAR()` needs at least two phrases) rather than a behavior a caller
    // observes.
    let fx = Fixture::open().await;
    let msg = fx
        .index(repo::NewMessage {
            body_text: Some("solo".to_owned()),
            ..Default::default()
        })
        .await;
    assert_eq!(fx.ids("solo").await, vec![msg]);
}

// ---------------------------------------------------------------------------
// Negation and semantic-mode exclusion
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_negated_term_excludes_messages_that_also_match_it() {
    let fx = Fixture::open().await;
    fx.index(repo::NewMessage {
        body_text: Some("invoice spam".to_owned()),
        ..Default::default()
    })
    .await;
    let only_required = fx
        .index(repo::NewMessage {
            body_text: Some("invoice attached".to_owned()),
            ..Default::default()
        })
        .await;

    assert_eq!(fx.ids("invoice -spam").await, vec![only_required]);
}

#[tokio::test]
async fn a_semantic_forced_term_alone_yields_nothing_lexically() {
    let fx = Fixture::open().await;
    fx.index(repo::NewMessage {
        body_text: Some("machine learning basics".to_owned()),
        ..Default::default()
    })
    .await;

    let parsed = query::ParsedQuery {
        terms: vec![Term {
            mode: Mode::Semantic,
            ..bare_term("machine")
        }],
        ..Default::default()
    };
    assert!(fx.retrieve(&parsed).await.is_empty());
}

#[tokio::test]
async fn a_semantic_forced_term_does_not_gate_the_lexical_match() {
    let fx = Fixture::open().await;
    // Matches "invoice" but not "widgets" — if the semantic-mode term were
    // still required lexically, this would be excluded.
    let matches = fx
        .index(repo::NewMessage {
            body_text: Some("invoice attached".to_owned()),
            ..Default::default()
        })
        .await;

    let parsed = query::ParsedQuery {
        terms: vec![
            bare_term("invoice"),
            Term {
                mode: Mode::Semantic,
                ..bare_term("widgets")
            },
        ],
        ..Default::default()
    };
    assert_eq!(
        fx.retrieve(&parsed)
            .await
            .into_iter()
            .map(|c| c.message_id)
            .collect::<Vec<_>>(),
        vec![matches]
    );
}

// ---------------------------------------------------------------------------
// Injection safety
// ---------------------------------------------------------------------------

#[tokio::test]
async fn metacharacters_in_a_single_term_cannot_change_the_query_shape() {
    let fx = Fixture::open().await;
    let noise = fx
        .index(repo::NewMessage {
            body_text: Some("completely unrelated filler content".to_owned()),
            ..Default::default()
        })
        .await;

    // Each of these, concatenated unescaped into an FTS5 `MATCH` string,
    // would be either a syntax error or a token that restructures the query
    // (`OR`/`AND`/`NOT`/`NEAR(` are keywords, `*` is the prefix operator, `"`
    // opens a string, `(`/`)` group). None of that may leak through once the
    // term is quoted — see `quote_fts_literal`.
    for text in [
        "OR",
        "AND",
        "NOT",
        "NEAR",
        "NEAR(x,1)",
        "*",
        "\"",
        "(",
        ")",
        "foo\"bar",
        "\"; DROP TABLE messages; --",
    ] {
        let parsed = query::ParsedQuery {
            terms: vec![bare_term(text)],
            ..Default::default()
        };
        let hits = fx
            .retrieve(&parsed)
            .await
            .into_iter()
            .map(|c| c.message_id)
            .collect::<Vec<_>>();
        assert!(
            !hits.contains(&noise),
            "term {text:?} must not turn into an operator that matches everything"
        );
    }

    // The table must still be there — this is a search index, not a SQL
    // console, but it costs nothing to check.
    assert!(fx.fts.len().await.unwrap() >= 1);
}

#[tokio::test]
async fn a_query_that_still_fails_to_parse_after_quoting_is_invalid_argument_on_every_path() {
    // Quoting defeats every FTS5 *keyword*, but an embedded NUL byte still
    // breaks FTS5's own string lexer inside a quoted literal (verified
    // against the real engine — this is a syntax error, never a
    // restructured query, so the injection defense above still holds). What
    // this test guards is the *error mapping*: the same user mistake must
    // report the same `InvalidArgument` whether or not a hard filter routes
    // the search through this module's own SQL instead of
    // `FtsIndex::search`.
    let fx = Fixture::open().await;
    fx.index(repo::NewMessage {
        body_text: Some("invoice".to_owned()),
        ..Default::default()
    })
    .await;

    let unmasked = ParsedQuery {
        terms: vec![bare_term("foo\0bar")],
        ..Default::default()
    };
    let err = fx
        .retriever
        .retrieve(&unmasked, 10, &no_cancel())
        .await
        .expect_err("an embedded NUL must not silently succeed");
    assert_eq!(err.reason(), ErrorReason::InvalidArgument);

    let masked = ParsedQuery {
        terms: vec![bare_term("foo\0bar")],
        filters: query::parse("is:unread").filters,
        ..Default::default()
    };
    let err = fx
        .retriever
        .retrieve(&masked, 10, &no_cancel())
        .await
        .expect_err("same NUL byte, this time with a hard filter present too");
    assert_eq!(
        err.reason(),
        ErrorReason::InvalidArgument,
        "must not degrade to Internal just because a filter was present"
    );
}

#[tokio::test]
async fn bare_fts5_keywords_are_matched_as_literal_words() {
    let fx = Fixture::open().await;
    for keyword in ["OR", "AND", "NOT", "NEAR"] {
        let containing = fx
            .index(repo::NewMessage {
                body_text: Some(format!("the term {keyword} appears right here")),
                ..Default::default()
            })
            .await;
        let not_containing = fx
            .index(repo::NewMessage {
                body_text: Some("nothing relevant in this one".to_owned()),
                ..Default::default()
            })
            .await;

        let parsed = query::ParsedQuery {
            terms: vec![bare_term(keyword)],
            ..Default::default()
        };
        let ids: Vec<i64> = fx
            .retrieve(&parsed)
            .await
            .into_iter()
            .map(|c| c.message_id)
            .collect();
        assert!(
            ids.contains(&containing),
            "{keyword:?} must match the message that literally contains it"
        );
        assert!(!ids.contains(&not_containing));
    }
}

#[test]
fn quote_fts_literal_wraps_plain_text() {
    assert_eq!(quote_fts_literal("invoice"), "\"invoice\"");
}

#[test]
fn quote_fts_literal_doubles_embedded_quotes() {
    assert_eq!(quote_fts_literal("say \"hi\""), "\"say \"\"hi\"\"\"");
}

// ---------------------------------------------------------------------------
// MatchExpr construction (pure, no database)
// ---------------------------------------------------------------------------

#[test]
fn match_expr_build_returns_none_for_a_pure_filter_query() {
    let parsed = query::parse("from:alice");
    assert!(MatchExpr::build(&parsed).is_none());
}

#[test]
fn match_expr_build_ands_terms_and_phrases() {
    let parsed = query::parse("alpha \"beta gamma\"");
    let expr = MatchExpr::build(&parsed).unwrap();
    assert_eq!(expr.full, "\"alpha\" AND \"beta gamma\"");
    assert!(
        expr.proximity.is_none(),
        "a phrase does not count toward the unquoted-term proximity probe"
    );
}

#[test]
fn match_expr_build_skips_punctuation_and_emoji_only_tokens() {
    // These would, quoted, match zero documents ever — ANDing one into
    // `required` must not zero out the rest of the query.
    let emoji_only = query::parse("budget 🎉");
    assert_eq!(MatchExpr::build(&emoji_only).unwrap().full, "\"budget\"");

    let bang_only = query::parse("report !");
    assert_eq!(MatchExpr::build(&bang_only).unwrap().full, "\"report\"");

    // A lone `-` survives `query::parse` as a literal term (its "bare
    // modifier" rule), not negation of nothing.
    let dash_only = query::parse("report -");
    assert_eq!(MatchExpr::build(&dash_only).unwrap().full, "\"report\"");

    let nothing_indexable = query::parse("🎉 !!! ---");
    assert!(
        MatchExpr::build(&nothing_indexable).is_none(),
        "no term survives, so there is nothing to rank"
    );
}

#[test]
fn match_expr_build_excludes_negated_terms_with_not() {
    let parsed = query::parse("alpha -beta");
    let expr = MatchExpr::build(&parsed).unwrap();
    assert_eq!(expr.full, "(\"alpha\") NOT (\"beta\")");
}

#[test]
fn match_expr_build_adds_near_only_with_two_or_more_bare_terms() {
    let one = MatchExpr::build(&query::parse("alpha")).unwrap();
    assert!(one.proximity.is_none());

    let two = MatchExpr::build(&query::parse("alpha beta")).unwrap();
    assert_eq!(
        two.proximity.as_deref(),
        Some("NEAR(\"alpha\" \"beta\", 10)")
    );

    // A negated term does not count toward the probe: only one eligible bare
    // term remains.
    let negated = MatchExpr::build(&query::parse("alpha -beta")).unwrap();
    assert!(negated.proximity.is_none());
}

// ---------------------------------------------------------------------------
// Hard filters as a candidate mask
// ---------------------------------------------------------------------------

#[tokio::test]
async fn from_filter_gates_by_sender() {
    let fx = Fixture::open().await;
    let from_alice = fx
        .index(repo::NewMessage {
            from_addr: Some("alice@example.com".to_owned()),
            body_text: Some("invoice".to_owned()),
            ..Default::default()
        })
        .await;
    fx.index(repo::NewMessage {
        from_addr: Some("bob@example.com".to_owned()),
        body_text: Some("invoice".to_owned()),
        ..Default::default()
    })
    .await;

    assert_eq!(fx.ids("invoice from:alice").await, vec![from_alice]);
}

#[tokio::test]
async fn to_and_cc_filters_gate_by_recipient() {
    let fx = Fixture::open().await;
    let to_alice = fx
        .index(repo::NewMessage {
            to_addrs: Some("alice@example.com".to_owned()),
            body_text: Some("invoice".to_owned()),
            ..Default::default()
        })
        .await;
    fx.index(repo::NewMessage {
        to_addrs: Some("bob@example.com".to_owned()),
        body_text: Some("invoice".to_owned()),
        ..Default::default()
    })
    .await;
    assert_eq!(fx.ids("invoice to:alice").await, vec![to_alice]);

    let cc_carol = fx
        .index(repo::NewMessage {
            cc_addrs: Some("carol@example.com".to_owned()),
            body_text: Some("invoice".to_owned()),
            ..Default::default()
        })
        .await;
    assert_eq!(fx.ids("invoice cc:carol").await, vec![cc_carol]);
}

#[tokio::test]
async fn subject_and_body_filters_gate_by_substring() {
    let fx = Fixture::open().await;
    let in_subject = fx
        .index(repo::NewMessage {
            subject: Some("Q3 invoice".to_owned()),
            body_text: Some("invoice".to_owned()),
            ..Default::default()
        })
        .await;
    fx.index(repo::NewMessage {
        subject: Some("unrelated".to_owned()),
        body_text: Some("invoice".to_owned()),
        ..Default::default()
    })
    .await;
    assert_eq!(fx.ids("invoice subject:Q3").await, vec![in_subject]);

    let specific_body = fx
        .index(repo::NewMessage {
            body_text: Some("invoice attached, see the yellow copy".to_owned()),
            ..Default::default()
        })
        .await;
    assert_eq!(fx.ids("invoice body:yellow").await, vec![specific_body]);
}

#[tokio::test]
async fn has_attachment_filter_gates_by_the_flag_column() {
    let fx = Fixture::open().await;
    let with_attachment = fx
        .index(repo::NewMessage {
            has_attachments: true,
            body_text: Some("invoice".to_owned()),
            ..Default::default()
        })
        .await;
    fx.index(repo::NewMessage {
        has_attachments: false,
        body_text: Some("invoice".to_owned()),
        ..Default::default()
    })
    .await;
    assert_eq!(
        fx.ids("invoice has:attachment").await,
        vec![with_attachment]
    );
}

#[tokio::test]
async fn filename_filter_gates_by_a_glob_over_attachments() {
    let fx = Fixture::open().await;
    let pdf = fx
        .index(repo::NewMessage {
            body_text: Some("invoice".to_owned()),
            ..Default::default()
        })
        .await;
    fx.attach(pdf, "receipt.pdf").await;
    let txt = fx
        .index(repo::NewMessage {
            body_text: Some("invoice".to_owned()),
            ..Default::default()
        })
        .await;
    fx.attach(txt, "notes.txt").await;

    assert_eq!(fx.ids("invoice filename:*.pdf").await, vec![pdf]);
}

#[tokio::test]
async fn filename_filter_is_case_insensitive() {
    let fx = Fixture::open().await;
    let pdf = fx
        .index(repo::NewMessage {
            body_text: Some("invoice".to_owned()),
            ..Default::default()
        })
        .await;
    fx.attach(pdf, "Report.PDF").await;

    assert_eq!(
        fx.ids("invoice filename:*.pdf").await,
        vec![pdf],
        "GLOB is case-sensitive by default; the filter must not be"
    );
}

#[tokio::test]
async fn larger_and_smaller_filters_gate_by_size() {
    let fx = Fixture::open().await;
    let small = fx
        .index(repo::NewMessage {
            size: Some(1_000),
            body_text: Some("invoice".to_owned()),
            ..Default::default()
        })
        .await;
    let big = fx
        .index(repo::NewMessage {
            size: Some(10_000_000),
            body_text: Some("invoice".to_owned()),
            ..Default::default()
        })
        .await;

    assert_eq!(fx.ids("invoice larger:1mb").await, vec![big]);
    assert_eq!(fx.ids("invoice smaller:1mb").await, vec![small]);
}

#[tokio::test]
async fn date_filters_gate_by_calendar_day_in_utc() {
    let fx = Fixture::open().await;
    let day = day_start("2024-06-15").unwrap();
    let on_day = fx
        .index(repo::NewMessage {
            date: Some(day + 3_600),
            body_text: Some("invoice".to_owned()),
            ..Default::default()
        })
        .await;
    let before_day = fx
        .index(repo::NewMessage {
            date: Some(day - 3_600),
            body_text: Some("invoice".to_owned()),
            ..Default::default()
        })
        .await;
    let after_day = fx
        .index(repo::NewMessage {
            date: Some(day + 90_000),
            body_text: Some("invoice".to_owned()),
            ..Default::default()
        })
        .await;

    assert_eq!(fx.ids("invoice on:2024-06-15").await, vec![on_day]);
    assert_eq!(fx.ids("invoice before:2024-06-15").await, vec![before_day]);
    assert_eq!(
        fx.ids("invoice after:2024-06-15")
            .await
            .into_iter()
            .collect::<HashSet<_>>(),
        HashSet::from([on_day, after_day]),
    );
    assert_eq!(
        fx.ids("invoice date:2024-06-14..2024-06-15")
            .await
            .into_iter()
            .collect::<HashSet<_>>(),
        HashSet::from([on_day, before_day]),
        "an inclusive range covering both the day before and the day itself"
    );
}

#[tokio::test]
async fn is_flag_filters_gate_by_imap_flags() {
    let fx = Fixture::open().await;
    let unread = fx
        .index(repo::NewMessage {
            body_text: Some("invoice".to_owned()),
            ..Default::default()
        })
        .await;
    let read = fx
        .index(repo::NewMessage {
            body_text: Some("invoice".to_owned()),
            ..Default::default()
        })
        .await;
    fx.flag(read, "\\Seen").await;
    let flagged = fx
        .index(repo::NewMessage {
            body_text: Some("invoice".to_owned()),
            ..Default::default()
        })
        .await;
    fx.flag(flagged, "\\Flagged").await;
    let replied = fx
        .index(repo::NewMessage {
            body_text: Some("invoice".to_owned()),
            ..Default::default()
        })
        .await;
    fx.flag(replied, "\\Answered").await;

    assert_eq!(
        fx.ids("invoice is:unread")
            .await
            .into_iter()
            .collect::<HashSet<_>>(),
        HashSet::from([unread, flagged, replied]),
        "every message without \\Seen"
    );
    assert_eq!(fx.ids("invoice is:read").await, vec![read]);
    assert_eq!(fx.ids("invoice is:flagged").await, vec![flagged]);
    assert_eq!(fx.ids("invoice is:replied").await, vec![replied]);
}

#[tokio::test]
async fn is_values_outside_the_documented_set_still_route_through_flags_when_backed() {
    let fx = Fixture::open().await;
    let draft = fx
        .index(repo::NewMessage {
            body_text: Some("invoice".to_owned()),
            ..Default::default()
        })
        .await;
    fx.flag(draft, "\\Draft").await;
    let junk = fx
        .index(repo::NewMessage {
            body_text: Some("invoice".to_owned()),
            ..Default::default()
        })
        .await;
    fx.flag(junk, "Junk").await;
    fx.index(repo::NewMessage {
        body_text: Some("invoice".to_owned()),
        ..Default::default()
    })
    .await;

    // `\Draft` is a raw IMAP system flag this grammar doesn't name directly,
    // recognized case-insensitively.
    assert_eq!(fx.ids("invoice is:Draft").await, vec![draft]);
    // "Junk" isn't a documented flag at all — `flags` is a general keyword
    // table, so it is matched as a literal custom keyword, not assumed to be
    // unbacked data.
    assert_eq!(fx.ids("invoice is:Junk").await, vec![junk]);
    // `pinned` really does name a concept with no backing data anywhere yet.
    assert!(fx.ids("invoice is:pinned").await.is_empty());
}

#[tokio::test]
async fn in_and_account_filters_gate_by_scope() {
    let fx = Fixture::open().await;
    let archive = fx.mailbox(fx.account_id, "Archive").await;
    let in_inbox = fx
        .index(repo::NewMessage {
            body_text: Some("invoice".to_owned()),
            ..Default::default()
        })
        .await;
    let in_archive = fx
        .index(repo::NewMessage {
            mailbox_id: archive,
            body_text: Some("invoice".to_owned()),
            ..Default::default()
        })
        .await;

    assert_eq!(fx.ids("invoice in:Archive").await, vec![in_archive]);
    assert_eq!(
        fx.ids("invoice in:inbox").await,
        vec![in_inbox],
        "case-insensitive"
    );

    let other_account = fx.account("Work").await;
    let other_mailbox = fx.mailbox(other_account, "INBOX").await;
    let in_work = fx
        .index(repo::NewMessage {
            account_id: other_account,
            mailbox_id: other_mailbox,
            body_text: Some("invoice".to_owned()),
            ..Default::default()
        })
        .await;
    assert_eq!(fx.ids("invoice account:Work").await, vec![in_work]);
}

#[tokio::test]
async fn thread_filter_gates_by_thread_id_and_a_malformed_id_matches_nothing() {
    let fx = Fixture::open().await;
    let thread_id = fx.thread().await;
    let in_thread = fx
        .index(repo::NewMessage {
            thread_id: Some(thread_id),
            body_text: Some("invoice".to_owned()),
            ..Default::default()
        })
        .await;
    fx.index(repo::NewMessage {
        body_text: Some("invoice".to_owned()),
        ..Default::default()
    })
    .await;

    assert_eq!(
        fx.ids(&format!("invoice thread:{thread_id}")).await,
        vec![in_thread]
    );
    assert!(
        fx.ids("invoice thread:not-a-number").await.is_empty(),
        "an id that can never denote a real thread excludes everything"
    );
}

#[tokio::test]
async fn multiple_hard_filters_conjoin() {
    let fx = Fixture::open().await;
    let matches_both = fx
        .index(repo::NewMessage {
            from_addr: Some("alice@example.com".to_owned()),
            body_text: Some("invoice".to_owned()),
            ..Default::default()
        })
        .await;
    fx.index(repo::NewMessage {
        from_addr: Some("bob@example.com".to_owned()),
        body_text: Some("invoice".to_owned()),
        ..Default::default()
    })
    .await;
    let read_alice = fx
        .index(repo::NewMessage {
            from_addr: Some("alice@example.com".to_owned()),
            body_text: Some("invoice".to_owned()),
            ..Default::default()
        })
        .await;
    fx.flag(read_alice, "\\Seen").await;

    assert_eq!(
        fx.ids("invoice from:alice is:unread").await,
        vec![matches_both]
    );
}

#[tokio::test]
async fn negating_a_filter_does_not_drop_messages_with_a_null_in_that_column() {
    // SQL's three-valued logic makes `NOT NULL` evaluate to `NULL`, not
    // `TRUE` — a naive negated clause would exclude every row where the
    // predicate can't be answered instead of including it. This message has
    // no Cc, no explicit sender, no size, no thread, and no date: every
    // column a negated filter below reads is actually NULL.
    let fx = Fixture::open().await;
    let msg = fx
        .index(repo::NewMessage {
            body_text: Some("invoice".to_owned()),
            ..Default::default()
        })
        .await;

    assert_eq!(fx.ids("invoice -cc:legal").await, vec![msg]);
    assert_eq!(fx.ids("invoice -from:nobody").await, vec![msg]);
    assert_eq!(fx.ids("invoice -subject:receipt").await, vec![msg]);
    assert_eq!(fx.ids("invoice -larger:1gb").await, vec![msg]);
    assert_eq!(fx.ids("invoice -thread:999999").await, vec![msg]);
    assert_eq!(fx.ids("invoice -before:2000-01-01").await, vec![msg]);
}

#[tokio::test]
async fn the_proximity_bonus_still_applies_under_a_hard_filter_mask() {
    // Proximity and a hard filter compose through the same query
    // (`search_ranked`) once either is present alone; this checks they still
    // compose correctly when both are present at once.
    let fx = Fixture::open().await;
    let filler = (1..=12)
        .map(|n| format!("filler{n}"))
        .collect::<Vec<_>>()
        .join(" ");
    let near = fx
        .index(repo::NewMessage {
            from_addr: Some("alice@example.com".to_owned()),
            body_text: Some(format!("alpha beta {filler}")),
            ..Default::default()
        })
        .await;
    let far = fx
        .index(repo::NewMessage {
            from_addr: Some("alice@example.com".to_owned()),
            body_text: Some(format!("alpha {filler} beta")),
            ..Default::default()
        })
        .await;
    fx.index(repo::NewMessage {
        from_addr: Some("bob@example.com".to_owned()),
        body_text: Some(format!("alpha beta {filler}")),
        ..Default::default()
    })
    .await;

    assert_eq!(
        fx.ids("alpha beta from:alice").await,
        vec![near, far],
        "from:alice gates out bob's message and the proximity bonus still separates alice's two"
    );
}

#[tokio::test]
async fn the_hard_filter_mask_is_applied_before_the_limit_not_after() {
    // If the mask were applied by filtering an already-limited page, this
    // would return nothing: with `limit=1` the unfiltered top hit is one of
    // the four read "noise" messages, every one of which `is:unread` must
    // exclude. Because the mask is baked into the same query the `LIMIT`
    // applies to, the one unread message that matches at all is still found.
    let fx = Fixture::open().await;
    for _ in 0..4 {
        let noise = fx
            .index(repo::NewMessage {
                subject: Some("budget budget budget budget budget".to_owned()),
                ..Default::default()
            })
            .await;
        fx.flag(noise, "\\Seen").await;
    }
    let target = fx
        .index(repo::NewMessage {
            body_text: Some("a passing mention of the budget".to_owned()),
            ..Default::default()
        })
        .await;

    let parsed = query::parse("budget is:unread");
    let hits = fx
        .retriever
        .retrieve(&parsed, 1, &no_cancel())
        .await
        .unwrap();
    assert_eq!(
        hits.into_iter().map(|c| c.message_id).collect::<Vec<_>>(),
        vec![target]
    );
}

#[tokio::test]
async fn a_filter_for_an_unbuilt_subsystem_excludes_everything_but_its_negation_does_not() {
    let fx = Fixture::open().await;
    let msg = fx
        .index(repo::NewMessage {
            body_text: Some("invoice".to_owned()),
            ..Default::default()
        })
        .await;

    for op in [
        "tag:work",
        "note:reminder",
        "ai:needs-reply",
        "has:note",
        "has:tag",
    ] {
        assert!(
            fx.ids(&format!("invoice {op}")).await.is_empty(),
            "{op} names a subsystem with no data yet, so it must exclude everything"
        );
        assert_eq!(
            fx.ids(&format!("invoice -{op}")).await,
            vec![msg],
            "negating it is vacuously true: nothing has it, so nothing is excluded"
        );
    }
}

#[tokio::test]
async fn an_unresolvable_date_value_does_not_gate_the_results() {
    let fx = Fixture::open().await;
    let msg = fx
        .index(repo::NewMessage {
            body_text: Some("invoice".to_owned()),
            ..Default::default()
        })
        .await;
    assert_eq!(
        fx.ids("invoice before:last-week").await,
        vec![msg],
        "a relative date this stage cannot resolve is skipped, not treated as excluding everything"
    );
}

// ---------------------------------------------------------------------------
// Cancellation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_cancelled_token_degrades_to_no_candidates_without_erroring() {
    // Lexical is the one retriever every query runs — task 28 threads
    // cancellation through it too (both the unmasked and the masked/
    // proximity path go through the same `search_ranked`, so one test
    // covers both).
    let fx = Fixture::open().await;
    fx.index(repo::NewMessage {
        body_text: Some("invoice".to_owned()),
        ..Default::default()
    })
    .await;

    let parsed = query::parse("invoice");
    let cancel = CancellationToken::new();
    cancel.cancel();
    let hits = fx.retriever.retrieve(&parsed, 100, &cancel).await.unwrap();
    assert!(hits.is_empty());
}

// ---------------------------------------------------------------------------
// Filter classification (pure, no database)
// ---------------------------------------------------------------------------

#[test]
fn day_start_parses_plain_iso_dates_only() {
    assert_eq!(day_start("1970-01-02"), Some(86_400));
    assert_eq!(
        day_start(" 1970-01-02 "),
        Some(86_400),
        "surrounding whitespace is trimmed"
    );
    assert_eq!(day_start("last-week"), None);
    assert_eq!(day_start("2024-13-40"), None, "an impossible calendar date");
    assert_eq!(
        day_start("2024-06-15T00:00:00Z"),
        None,
        "only the plain YYYY-MM-DD form is accepted at this stage"
    );
}

#[test]
fn compile_filters_excludes_everything_only_for_a_positive_unbacked_filter() {
    assert!(matches!(
        compile_filters(&query::parse("tag:work").filters),
        FilterMask::ExcludesEverything
    ));
    assert!(!matches!(
        compile_filters(&query::parse("-tag:work").filters),
        FilterMask::ExcludesEverything
    ));
    assert!(!matches!(
        compile_filters(&query::parse("from:alice").filters),
        FilterMask::ExcludesEverything
    ));
}

#[test]
fn compile_filters_excludes_everything_for_a_non_numeric_thread_id() {
    assert!(matches!(
        compile_filters(&query::parse("thread:not-a-number").filters),
        FilterMask::ExcludesEverything
    ));
}

#[test]
fn compile_filters_is_unconstrained_with_no_filters() {
    assert!(matches!(
        compile_filters(&query::parse("invoice").filters),
        FilterMask::Unconstrained
    ));
}
