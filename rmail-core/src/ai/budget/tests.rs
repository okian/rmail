//! The properties the budget enforcer exists for: a hard cap stops a call
//! *before* the provider is reached (proved by counting calls into a mock
//! provider, not by inspecting the error afterwards), a soft cap downgrades
//! rather than blocks, both boundaries land exactly on `>=`, the bulk
//! sub-budget insulates interactive work from a backlog and vice versa, the
//! more restrictive of the global and per-account caps wins, and yesterday's
//! spend leaves today's daily cap alone while still counting against the
//! month.
//!
//! Every spend figure a test sets up is written straight into `ai_ledger`
//! with a chosen `created_at`, because that is the only way to test window
//! arithmetic at all: `record_call` stamps `Utc::now()`, so a suite that went
//! through it could only ever exercise "today". The rows are shaped exactly
//! as `record_call_charged` writes them.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::TimeZone;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::ai::provider::{
    ChatMessage, ChatRequest, ChatResponse, Provider, ProviderStream, Role, StopReason, Usage,
};
use crate::ai::queue::{
    AiLease, AiQueue, AiWorkerPool, MessageContent, NewAiJob, PassHandler, QueueOptions,
    PRIORITY_BACKFILL, PRIORITY_NORMAL, PRIORITY_RECENT,
};
use crate::ai::PolicyEngine;
use crate::config::{AiBudget, AiModels, AiPolicyMode, AiPrivacy};
use crate::events::{EventLog, Retention};
use crate::repo;

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// 2026-08-05T12:00:00Z — a fixed "now" so window arithmetic is checked
/// against a chosen instant rather than whenever the suite happens to run.
fn now_ts() -> i64 {
    chrono::Utc
        .with_ymd_and_hms(2026, 8, 5, 12, 0, 0)
        .single()
        .expect("2026-08-05T12:00:00Z is a real instant")
        .timestamp()
}

/// Offset from [`now_ts`] by whole days.
fn days_before(days: i64) -> i64 {
    now_ts() - days * 86_400
}

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

struct Fixture {
    db: Database,
    path: PathBuf,
    account_id: i64,
    mailbox_id: i64,
}

impl Fixture {
    async fn open() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-budget-{pid}-{n}.db"));
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
        Self {
            db,
            path,
            account_id,
            mailbox_id,
        }
    }

    async fn message(&self) -> i64 {
        let account_id = self.account_id;
        let mailbox_id = self.mailbox_id;
        let uid = i64::from(COUNTER.fetch_add(1, Ordering::Relaxed) as u32).max(1);
        self.db
            .write(move |c| {
                repo::insert_message(
                    c,
                    &repo::NewMessage {
                        account_id,
                        mailbox_id,
                        uid,
                        uidvalidity: 1,
                        subject: Some("Test message".to_owned()),
                        body_text: Some("body".to_owned()),
                        ..Default::default()
                    },
                )
            })
            .await
            .unwrap()
    }

    /// Append a ledger row with a chosen timestamp, account, cost, token
    /// count, and work class — the raw shape `record_call_charged` writes.
    async fn spend(
        &self,
        created_at: i64,
        account_id: Option<i64>,
        usd: f64,
        tokens: i64,
        class: WorkClass,
    ) {
        let class = class.as_str().to_owned();
        self.db
            .write(move |conn| {
                conn.execute(
                    "INSERT INTO ai_ledger (
                         created_at, account_id, model, input_tokens, output_tokens,
                         cache_creation_input_tokens, cache_read_input_tokens, cost_usd,
                         redaction_level, latency_ms, payload_sha256, status, work_class
                     ) VALUES (?1, ?2, 'claude-opus-4-8', ?3, 0, 0, 0, ?4, 'none', 1, X'00', 'ok', ?5)",
                    rusqlite::params![created_at, account_id, tokens, usd, class],
                )?;
                Ok(())
            })
            .await
            .unwrap();
    }

    /// The single `work_class` value stored for the only ledger row.
    async fn only_work_class(&self) -> String {
        self.db
            .read(|conn| conn.query_row("SELECT work_class FROM ai_ledger", [], |row| row.get(0)))
            .await
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.path.display())));
        }
    }
}

/// `ai.limits` with the cycle-level caps set far out of the way, so only the
/// budget row a test stores can decide anything.
fn open_limits() -> AiLimits {
    AiLimits {
        daily_token_cap: u64::MAX,
        daily_cost_cap_usd: 1_000_000.0,
        monthly_cost_cap_usd: 1_000_000.0,
        ..AiLimits::default()
    }
}

async fn evaluate(
    fx: &Fixture,
    limits: &AiLimits,
    account_id: i64,
    model: &str,
    work_class: WorkClass,
) -> BudgetVerdict {
    BudgetEnforcer { db: &fx.db, limits }
        .evaluate(&BudgetRequest {
            account_id,
            model,
            work_class,
            now: now_ts(),
        })
        .await
        .unwrap()
}

/// A budget capping only the daily dollar dimension.
fn daily_usd_budget(account_id: i64, class: BudgetClass, soft: i64, hard: i64) -> Budget {
    Budget {
        account_id,
        class,
        caps: BudgetCaps {
            daily: WindowCaps {
                soft_usd_micros: Some(soft),
                hard_usd_micros: Some(hard),
                ..WindowCaps::default()
            },
            monthly: WindowCaps::default(),
        },
    }
}

/// The model a downgrade verdict picked, or `None` for any other verdict.
///
/// Accessors rather than `let ... else { panic!(...) }` so the assertions
/// below read as comparisons — and so the workspace's `clippy::panic` denial
/// stays intact rather than being loosened to let a test compile.
fn downgraded_to(verdict: &BudgetVerdict) -> Option<&str> {
    match verdict {
        BudgetVerdict::Downgrade { model, .. } => Some(model),
        _ => None,
    }
}

/// The reason a block verdict gave, or `None` for any other verdict.
fn block_reason(verdict: &BudgetVerdict) -> Option<&str> {
    match verdict {
        BudgetVerdict::Block { reason, .. } => Some(reason),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Boundaries: `>=` on both caps, in both dimensions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn soft_cap_is_open_one_micro_dollar_below_and_downgrades_exactly_at_it() {
    let fx = Fixture::open().await;
    let limits = open_limits();
    // Soft at $1.000000, hard at $9.000000.
    set_budget(
        &fx.db,
        &daily_usd_budget(GLOBAL_ACCOUNT_ID, BudgetClass::All, 1_000_000, 9_000_000),
    )
    .await
    .unwrap();

    // One micro-dollar short of the soft cap: nothing has happened yet.
    fx.spend(now_ts(), None, 0.999_999, 0, WorkClass::Interactive)
        .await;
    assert_eq!(
        evaluate(
            &fx,
            &limits,
            fx.account_id,
            "claude-opus-4-8",
            WorkClass::Interactive
        )
        .await,
        BudgetVerdict::Allow,
        "spend one micro-dollar below the soft cap must not downgrade"
    );

    // Exactly at it. `>=` means the cap is consumed, not merely approached.
    fx.spend(now_ts(), None, 0.000_001, 0, WorkClass::Interactive)
        .await;
    assert!(
        matches!(
            evaluate(
                &fx,
                &limits,
                fx.account_id,
                "claude-opus-4-8",
                WorkClass::Interactive
            )
            .await,
            BudgetVerdict::Downgrade { .. }
        ),
        "spend exactly at the soft cap must downgrade"
    );
}

#[tokio::test]
async fn soft_cap_still_downgrades_one_unit_past_it() {
    let fx = Fixture::open().await;
    let limits = open_limits();
    set_budget(
        &fx.db,
        &daily_usd_budget(GLOBAL_ACCOUNT_ID, BudgetClass::All, 1_000_000, 9_000_000),
    )
    .await
    .unwrap();
    fx.spend(now_ts(), None, 1.000_001, 0, WorkClass::Interactive)
        .await;

    assert!(matches!(
        evaluate(
            &fx,
            &limits,
            fx.account_id,
            "claude-opus-4-8",
            WorkClass::Interactive
        )
        .await,
        BudgetVerdict::Downgrade { .. }
    ));
}

#[tokio::test]
async fn hard_cap_is_open_one_micro_dollar_below_and_blocks_exactly_at_it() {
    let fx = Fixture::open().await;
    let limits = open_limits();
    // No soft cap at all, so only the hard boundary is under test.
    set_budget(
        &fx.db,
        &Budget {
            account_id: GLOBAL_ACCOUNT_ID,
            class: BudgetClass::All,
            caps: BudgetCaps {
                daily: WindowCaps {
                    hard_usd_micros: Some(2_000_000),
                    ..WindowCaps::default()
                },
                monthly: WindowCaps::default(),
            },
        },
    )
    .await
    .unwrap();

    fx.spend(now_ts(), None, 1.999_999, 0, WorkClass::Interactive)
        .await;
    assert!(
        !matches!(
            evaluate(
                &fx,
                &limits,
                fx.account_id,
                "claude-haiku-4-5",
                WorkClass::Interactive
            )
            .await,
            BudgetVerdict::Block { .. }
        ),
        "spend one micro-dollar below the hard cap must not block"
    );

    fx.spend(now_ts(), None, 0.000_001, 0, WorkClass::Interactive)
        .await;
    assert!(
        matches!(
            evaluate(
                &fx,
                &limits,
                fx.account_id,
                "claude-haiku-4-5",
                WorkClass::Interactive
            )
            .await,
            BudgetVerdict::Block { .. }
        ),
        "spend exactly at the hard cap must block"
    );
}

#[tokio::test]
async fn hard_cap_still_blocks_one_unit_past_it() {
    let fx = Fixture::open().await;
    let limits = open_limits();
    set_budget(
        &fx.db,
        &Budget {
            account_id: GLOBAL_ACCOUNT_ID,
            class: BudgetClass::All,
            caps: BudgetCaps {
                daily: WindowCaps {
                    hard_usd_micros: Some(2_000_000),
                    ..WindowCaps::default()
                },
                monthly: WindowCaps::default(),
            },
        },
    )
    .await
    .unwrap();
    fx.spend(now_ts(), None, 2.000_001, 0, WorkClass::Interactive)
        .await;

    assert!(matches!(
        evaluate(
            &fx,
            &limits,
            fx.account_id,
            "claude-opus-4-8",
            WorkClass::Interactive
        )
        .await,
        BudgetVerdict::Block { .. }
    ));
}

#[tokio::test]
async fn token_caps_use_the_same_boundary_as_dollar_caps() {
    let fx = Fixture::open().await;
    let limits = open_limits();
    set_budget(
        &fx.db,
        &Budget {
            account_id: GLOBAL_ACCOUNT_ID,
            class: BudgetClass::All,
            caps: BudgetCaps {
                daily: WindowCaps {
                    soft_tokens: Some(100),
                    hard_tokens: Some(200),
                    ..WindowCaps::default()
                },
                monthly: WindowCaps::default(),
            },
        },
    )
    .await
    .unwrap();

    fx.spend(now_ts(), None, 0.0, 99, WorkClass::Interactive)
        .await;
    assert_eq!(
        evaluate(
            &fx,
            &limits,
            fx.account_id,
            "claude-opus-4-8",
            WorkClass::Interactive
        )
        .await,
        BudgetVerdict::Allow,
        "99 of a 100-token soft cap is still open"
    );

    fx.spend(now_ts(), None, 0.0, 1, WorkClass::Interactive)
        .await;
    assert!(
        matches!(
            evaluate(
                &fx,
                &limits,
                fx.account_id,
                "claude-opus-4-8",
                WorkClass::Interactive
            )
            .await,
            BudgetVerdict::Downgrade { .. }
        ),
        "exactly 100 tokens consumes the 100-token soft cap"
    );

    fx.spend(now_ts(), None, 0.0, 99, WorkClass::Interactive)
        .await;
    assert!(
        !matches!(
            evaluate(
                &fx,
                &limits,
                fx.account_id,
                "claude-opus-4-8",
                WorkClass::Interactive
            )
            .await,
            BudgetVerdict::Block { .. }
        ),
        "199 of a 200-token hard cap is not yet blocked"
    );

    fx.spend(now_ts(), None, 0.0, 1, WorkClass::Interactive)
        .await;
    assert!(
        matches!(
            evaluate(
                &fx,
                &limits,
                fx.account_id,
                "claude-opus-4-8",
                WorkClass::Interactive
            )
            .await,
            BudgetVerdict::Block { .. }
        ),
        "exactly 200 tokens consumes the 200-token hard cap"
    );
}

// ---------------------------------------------------------------------------
// The downgrade ladder
// ---------------------------------------------------------------------------

#[tokio::test]
async fn soft_cap_steps_the_model_exactly_one_rung_down() {
    let fx = Fixture::open().await;
    let limits = open_limits();
    set_budget(
        &fx.db,
        &daily_usd_budget(GLOBAL_ACCOUNT_ID, BudgetClass::All, 1_000_000, 9_000_000),
    )
    .await
    .unwrap();
    fx.spend(now_ts(), None, 5.0, 0, WorkClass::Interactive)
        .await;

    let ladder = &limits.budget.ladder;
    for (requested, expected) in [
        ("claude-opus-4-8", ladder.sonnet.clone()),
        ("claude-opus-5", ladder.sonnet.clone()),
        ("claude-sonnet-5", ladder.haiku.clone()),
    ] {
        let verdict = evaluate(
            &fx,
            &limits,
            fx.account_id,
            requested,
            WorkClass::Interactive,
        )
        .await;
        assert_eq!(
            downgraded_to(&verdict),
            Some(expected.as_str()),
            "{requested} must step down exactly one rung; got {verdict:?}"
        );
    }
}

#[tokio::test]
async fn a_soft_cap_never_blocks_a_model_already_on_the_bottom_rung() {
    let fx = Fixture::open().await;
    let limits = open_limits();
    set_budget(
        &fx.db,
        &daily_usd_budget(GLOBAL_ACCOUNT_ID, BudgetClass::All, 1_000_000, 9_000_000),
    )
    .await
    .unwrap();
    fx.spend(now_ts(), None, 5.0, 0, WorkClass::Interactive)
        .await;

    // Haiku has nothing cheaper to fall back to, and an id naming no family
    // cannot be placed on the ladder at all. A soft cap downgrades or does
    // nothing — it must never turn into a block.
    for model in ["claude-haiku-4-5", "some-local-model"] {
        assert_eq!(
            evaluate(&fx, &limits, fx.account_id, model, WorkClass::Interactive).await,
            BudgetVerdict::Allow,
            "a soft cap must not block {model}"
        );
    }
}

#[tokio::test]
async fn two_breached_soft_caps_still_step_only_one_rung() {
    let fx = Fixture::open().await;
    let limits = open_limits();
    set_budget(
        &fx.db,
        &Budget {
            account_id: GLOBAL_ACCOUNT_ID,
            class: BudgetClass::All,
            caps: BudgetCaps {
                daily: WindowCaps {
                    soft_usd_micros: Some(1_000_000),
                    hard_usd_micros: Some(90_000_000),
                    ..WindowCaps::default()
                },
                monthly: WindowCaps {
                    soft_usd_micros: Some(1_000_000),
                    hard_usd_micros: Some(90_000_000),
                    ..WindowCaps::default()
                },
            },
        },
    )
    .await
    .unwrap();
    fx.spend(now_ts(), None, 5.0, 0, WorkClass::Interactive)
        .await;

    let verdict = evaluate(
        &fx,
        &limits,
        fx.account_id,
        "claude-opus-4-8",
        WorkClass::Interactive,
    )
    .await;
    assert_eq!(
        downgraded_to(&verdict),
        Some(limits.budget.ladder.sonnet.as_str()),
        "breaching the daily and monthly soft caps at once is still one rung, not two; \
         got {verdict:?}"
    );
}

#[tokio::test]
async fn a_hard_cap_outranks_a_soft_cap_breached_at_the_same_time() {
    let fx = Fixture::open().await;
    let limits = open_limits();
    // The account's soft cap is breached; the global hard cap is exhausted.
    // Deny-wins must pick the block.
    set_budget(
        &fx.db,
        &Budget {
            account_id: GLOBAL_ACCOUNT_ID,
            class: BudgetClass::All,
            caps: BudgetCaps {
                daily: WindowCaps {
                    hard_usd_micros: Some(2_000_000),
                    ..WindowCaps::default()
                },
                monthly: WindowCaps::default(),
            },
        },
    )
    .await
    .unwrap();
    set_budget(
        &fx.db,
        &daily_usd_budget(fx.account_id, BudgetClass::All, 1_000_000, 500_000_000),
    )
    .await
    .unwrap();
    fx.spend(
        now_ts(),
        Some(fx.account_id),
        3.0,
        0,
        WorkClass::Interactive,
    )
    .await;

    assert!(matches!(
        evaluate(
            &fx,
            &limits,
            fx.account_id,
            "claude-opus-4-8",
            WorkClass::Interactive
        )
        .await,
        BudgetVerdict::Block { .. }
    ));
}

// ---------------------------------------------------------------------------
// Global vs. per-account: the more restrictive wins
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_generous_account_cap_cannot_override_an_exhausted_global_cap() {
    let fx = Fixture::open().await;
    let limits = open_limits();
    set_budget(
        &fx.db,
        &Budget {
            account_id: GLOBAL_ACCOUNT_ID,
            class: BudgetClass::All,
            caps: BudgetCaps {
                daily: WindowCaps {
                    hard_usd_micros: Some(1_000_000),
                    ..WindowCaps::default()
                },
                monthly: WindowCaps::default(),
            },
        },
    )
    .await
    .unwrap();
    // A thousand dollars a day for this account specifically — and it must
    // not matter.
    set_budget(
        &fx.db,
        &Budget {
            account_id: fx.account_id,
            class: BudgetClass::All,
            caps: BudgetCaps {
                daily: WindowCaps {
                    hard_usd_micros: Some(1_000_000_000),
                    ..WindowCaps::default()
                },
                monthly: WindowCaps::default(),
            },
        },
    )
    .await
    .unwrap();
    fx.spend(
        now_ts(),
        Some(fx.account_id),
        1.5,
        0,
        WorkClass::Interactive,
    )
    .await;

    let verdict = evaluate(
        &fx,
        &limits,
        fx.account_id,
        "claude-opus-4-8",
        WorkClass::Interactive,
    )
    .await;
    let reason = block_reason(&verdict).unwrap_or_default();
    assert!(
        reason.contains("global"),
        "the exhausted global cap must win and the reason must name it; got {verdict:?}"
    );
}

#[tokio::test]
async fn an_exhausted_account_cap_blocks_while_the_global_budget_is_wide_open() {
    let fx = Fixture::open().await;
    let limits = open_limits();
    set_budget(
        &fx.db,
        &Budget {
            account_id: fx.account_id,
            class: BudgetClass::All,
            caps: BudgetCaps {
                daily: WindowCaps {
                    hard_usd_micros: Some(1_000_000),
                    ..WindowCaps::default()
                },
                monthly: WindowCaps::default(),
            },
        },
    )
    .await
    .unwrap();
    fx.spend(
        now_ts(),
        Some(fx.account_id),
        1.5,
        0,
        WorkClass::Interactive,
    )
    .await;

    let verdict = evaluate(
        &fx,
        &limits,
        fx.account_id,
        "claude-opus-4-8",
        WorkClass::Interactive,
    )
    .await;
    let reason = block_reason(&verdict).unwrap_or_default();
    assert!(
        reason.contains(&format!("account {}", fx.account_id)),
        "the exhausted account cap must block and the reason must name it; got {verdict:?}"
    );

    // A *different* account is untouched by it: the cap is per-account, not
    // a global one wearing an account's name.
    assert_eq!(
        evaluate(
            &fx,
            &limits,
            4242,
            "claude-opus-4-8",
            WorkClass::Interactive
        )
        .await,
        BudgetVerdict::Allow
    );
}

// ---------------------------------------------------------------------------
// The bulk sub-budget, in both directions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bulk_exhausting_its_sub_budget_does_not_starve_interactive_work() {
    let fx = Fixture::open().await;
    let limits = open_limits();
    // $10/day overall, $1/day of it for bulk.
    set_budget(
        &fx.db,
        &Budget {
            account_id: GLOBAL_ACCOUNT_ID,
            class: BudgetClass::All,
            caps: BudgetCaps {
                daily: WindowCaps {
                    hard_usd_micros: Some(10_000_000),
                    ..WindowCaps::default()
                },
                monthly: WindowCaps::default(),
            },
        },
    )
    .await
    .unwrap();
    set_budget(
        &fx.db,
        &Budget {
            account_id: GLOBAL_ACCOUNT_ID,
            class: BudgetClass::Bulk,
            caps: BudgetCaps {
                daily: WindowCaps {
                    hard_usd_micros: Some(1_000_000),
                    ..WindowCaps::default()
                },
                monthly: WindowCaps::default(),
            },
        },
    )
    .await
    .unwrap();

    fx.spend(now_ts(), None, 1.0, 0, WorkClass::Bulk).await;

    assert!(
        matches!(
            evaluate(
                &fx,
                &limits,
                fx.account_id,
                "claude-opus-4-8",
                WorkClass::Bulk
            )
            .await,
            BudgetVerdict::Block { .. }
        ),
        "bulk has spent its whole sub-budget and must stop"
    );
    assert_eq!(
        evaluate(
            &fx,
            &limits,
            fx.account_id,
            "claude-opus-4-8",
            WorkClass::Interactive
        )
        .await,
        BudgetVerdict::Allow,
        "interactive work still has $9 of the shared budget and must not be starved"
    );
}

#[tokio::test]
async fn interactive_spend_does_not_consume_the_bulk_sub_budget() {
    let fx = Fixture::open().await;
    let limits = open_limits();
    set_budget(
        &fx.db,
        &Budget {
            account_id: GLOBAL_ACCOUNT_ID,
            class: BudgetClass::All,
            caps: BudgetCaps {
                daily: WindowCaps {
                    hard_usd_micros: Some(10_000_000),
                    ..WindowCaps::default()
                },
                monthly: WindowCaps::default(),
            },
        },
    )
    .await
    .unwrap();
    set_budget(
        &fx.db,
        &Budget {
            account_id: GLOBAL_ACCOUNT_ID,
            class: BudgetClass::Bulk,
            caps: BudgetCaps {
                daily: WindowCaps {
                    hard_usd_micros: Some(1_000_000),
                    ..WindowCaps::default()
                },
                monthly: WindowCaps::default(),
            },
        },
    )
    .await
    .unwrap();

    // Five dollars of user-driven analysis — half the shared budget, and
    // several times the bulk sub-budget.
    fx.spend(now_ts(), None, 5.0, 0, WorkClass::Interactive)
        .await;

    assert_eq!(
        evaluate(
            &fx,
            &limits,
            fx.account_id,
            "claude-opus-4-8",
            WorkClass::Bulk
        )
        .await,
        BudgetVerdict::Allow,
        "the backlog's reserved share must survive a busy day of interactive work"
    );
}

#[tokio::test]
async fn bulk_is_still_bound_by_the_shared_budget_it_is_carved_out_of() {
    let fx = Fixture::open().await;
    let limits = open_limits();
    // A bulk sub-budget larger than the budget it sits inside: the `all`
    // check must still bite, or "sub-budget" would mean nothing.
    set_budget(
        &fx.db,
        &Budget {
            account_id: GLOBAL_ACCOUNT_ID,
            class: BudgetClass::All,
            caps: BudgetCaps {
                daily: WindowCaps {
                    hard_usd_micros: Some(2_000_000),
                    ..WindowCaps::default()
                },
                monthly: WindowCaps::default(),
            },
        },
    )
    .await
    .unwrap();
    set_budget(
        &fx.db,
        &Budget {
            account_id: GLOBAL_ACCOUNT_ID,
            class: BudgetClass::Bulk,
            caps: BudgetCaps {
                daily: WindowCaps {
                    hard_usd_micros: Some(500_000_000),
                    ..WindowCaps::default()
                },
                monthly: WindowCaps::default(),
            },
        },
    )
    .await
    .unwrap();
    fx.spend(now_ts(), None, 3.0, 0, WorkClass::Interactive)
        .await;

    assert!(matches!(
        evaluate(
            &fx,
            &limits,
            fx.account_id,
            "claude-opus-4-8",
            WorkClass::Bulk
        )
        .await,
        BudgetVerdict::Block { .. }
    ));
}

#[tokio::test]
async fn the_bulk_sub_budget_defaults_to_a_share_of_the_scope_with_no_row_stored() {
    let fx = Fixture::open().await;
    // $10/day globally from config, `bulk_share` 0.5 by default → bulk may
    // spend $5 before it stops, while interactive still has the full $10.
    let limits = AiLimits {
        daily_token_cap: u64::MAX,
        daily_cost_cap_usd: 10.0,
        monthly_cost_cap_usd: 1_000_000.0,
        ..AiLimits::default()
    };
    fx.spend(now_ts(), None, 5.0, 0, WorkClass::Bulk).await;

    assert!(
        matches!(
            evaluate(
                &fx,
                &limits,
                fx.account_id,
                "claude-haiku-4-5",
                WorkClass::Bulk
            )
            .await,
            BudgetVerdict::Block { .. }
        ),
        "the derived bulk sub-budget is half of $10 and is now spent"
    );
    // Interactive is over the derived *soft* cap ($8 of $10 is not yet
    // reached at $5), so it is fully open.
    assert_eq!(
        evaluate(
            &fx,
            &limits,
            fx.account_id,
            "claude-haiku-4-5",
            WorkClass::Interactive
        )
        .await,
        BudgetVerdict::Allow
    );
}

// ---------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------

#[tokio::test]
async fn yesterdays_spend_leaves_todays_daily_cap_alone_but_counts_for_the_month() {
    let fx = Fixture::open().await;
    let limits = open_limits();
    set_budget(
        &fx.db,
        &Budget {
            account_id: GLOBAL_ACCOUNT_ID,
            class: BudgetClass::All,
            caps: BudgetCaps {
                daily: WindowCaps {
                    hard_usd_micros: Some(5_000_000),
                    ..WindowCaps::default()
                },
                monthly: WindowCaps {
                    hard_usd_micros: Some(6_000_000),
                    ..WindowCaps::default()
                },
            },
        },
    )
    .await
    .unwrap();

    // $4 spent yesterday. Same calendar month (the 4th of August), a
    // different calendar day.
    fx.spend(days_before(1), None, 4.0, 0, WorkClass::Interactive)
        .await;
    assert_eq!(
        evaluate(
            &fx,
            &limits,
            fx.account_id,
            "claude-haiku-4-5",
            WorkClass::Interactive
        )
        .await,
        BudgetVerdict::Allow,
        "yesterday's $4 must not consume today's $5 daily cap"
    );

    // Another $2 today. Today's total is $2 (under $5), but the month is now
    // at $6 — exactly the monthly cap.
    fx.spend(now_ts(), None, 2.0, 0, WorkClass::Interactive)
        .await;
    let verdict = evaluate(
        &fx,
        &limits,
        fx.account_id,
        "claude-haiku-4-5",
        WorkClass::Interactive,
    )
    .await;
    let reason = block_reason(&verdict).unwrap_or_default();
    assert!(
        reason.contains("monthly"),
        "the daily window is still open; only the month is exhausted; got {verdict:?}"
    );
}

#[tokio::test]
async fn last_months_spend_counts_against_neither_window() {
    let fx = Fixture::open().await;
    let limits = open_limits();
    set_budget(
        &fx.db,
        &Budget {
            account_id: GLOBAL_ACCOUNT_ID,
            class: BudgetClass::All,
            caps: BudgetCaps {
                daily: WindowCaps {
                    hard_usd_micros: Some(1_000_000),
                    ..WindowCaps::default()
                },
                monthly: WindowCaps {
                    hard_usd_micros: Some(1_000_000),
                    ..WindowCaps::default()
                },
            },
        },
    )
    .await
    .unwrap();
    // 2026-07-06, comfortably inside the previous calendar month.
    fx.spend(days_before(30), None, 500.0, 0, WorkClass::Interactive)
        .await;

    assert_eq!(
        evaluate(
            &fx,
            &limits,
            fx.account_id,
            "claude-haiku-4-5",
            WorkClass::Interactive
        )
        .await,
        BudgetVerdict::Allow,
        "a previous month's spend is outside both windows"
    );
}

// ---------------------------------------------------------------------------
// Classification and attribution
// ---------------------------------------------------------------------------

#[test]
fn queue_priority_decides_the_work_class() {
    let bulk_at = AiBudget::default().bulk_priority;
    assert_eq!(
        WorkClass::for_priority(PRIORITY_RECENT, bulk_at),
        WorkClass::Interactive
    );
    assert_eq!(
        WorkClass::for_priority(PRIORITY_NORMAL, bulk_at),
        WorkClass::Interactive
    );
    assert_eq!(
        WorkClass::for_priority(bulk_at - 1, bulk_at),
        WorkClass::Interactive,
        "one below the threshold is still interactive"
    );
    assert_eq!(
        WorkClass::for_priority(PRIORITY_BACKFILL, bulk_at),
        WorkClass::Bulk
    );
}

#[tokio::test]
async fn record_call_defaults_to_interactive_and_record_call_charged_says_otherwise() {
    let fx = Fixture::open().await;
    let message_id = fx.message().await;

    let record = || crate::ai::CallRecord {
        account_id: Some(fx.account_id),
        message_id: Some(message_id),
        request_id: None,
        model: "claude-haiku-4-5".to_owned(),
        pass: Some("triage".to_owned()),
        usage: Usage {
            input_tokens: 10,
            output_tokens: 5,
            ..Usage::default()
        },
        redaction_level: "none".to_owned(),
        latency: std::time::Duration::from_millis(1),
        payload: b"payload",
        outcome: crate::ai::CallOutcome::Ok,
    };

    crate::ai::record_call(&fx.db, record()).await.unwrap();
    assert_eq!(
        fx.only_work_class().await,
        "interactive",
        "the pre-budget entry point must charge interactive, not leave the column meaningless"
    );

    crate::ai::record_call_charged(&fx.db, record(), 1.0, WorkClass::Bulk)
        .await
        .unwrap();
    let classes: Vec<String> = fx
        .db
        .read(|conn| {
            let mut stmt = conn.prepare("SELECT work_class FROM ai_ledger ORDER BY id")?;
            let rows = stmt
                .query_map([], |row| row.get(0))?
                .collect::<rusqlite::Result<Vec<String>>>()?;
            Ok(rows)
        })
        .await
        .unwrap();
    assert_eq!(classes, vec!["interactive".to_owned(), "bulk".to_owned()]);
}

// ---------------------------------------------------------------------------
// Storage, validation, and reporting
// ---------------------------------------------------------------------------

#[tokio::test]
async fn set_budget_upserts_and_get_budget_reads_it_back() {
    let fx = Fixture::open().await;
    let first = daily_usd_budget(fx.account_id, BudgetClass::All, 1_000_000, 2_000_000);
    set_budget(&fx.db, &first).await.unwrap();
    assert_eq!(
        get_budget(&fx.db, fx.account_id, BudgetClass::All)
            .await
            .unwrap(),
        Some(first)
    );

    let second = daily_usd_budget(fx.account_id, BudgetClass::All, 3_000_000, 4_000_000);
    set_budget(&fx.db, &second).await.unwrap();
    assert_eq!(
        get_budget(&fx.db, fx.account_id, BudgetClass::All)
            .await
            .unwrap(),
        Some(second),
        "a second SetBudget for the same scope replaces the first rather than colliding"
    );
    assert_eq!(
        get_budget(&fx.db, fx.account_id, BudgetClass::Bulk)
            .await
            .unwrap(),
        None,
        "the bulk sub-budget is a separate row and was never set"
    );
}

#[tokio::test]
async fn set_budget_rejects_caps_a_client_cannot_have_meant() {
    let fx = Fixture::open().await;

    let soft_at_hard = daily_usd_budget(GLOBAL_ACCOUNT_ID, BudgetClass::All, 5_000_000, 5_000_000);
    let err = set_budget(&fx.db, &soft_at_hard).await.unwrap_err();
    assert_eq!(err.reason(), crate::ErrorReason::InvalidArgument);

    let negative = Budget {
        account_id: GLOBAL_ACCOUNT_ID,
        class: BudgetClass::All,
        caps: BudgetCaps {
            daily: WindowCaps {
                hard_tokens: Some(-1),
                ..WindowCaps::default()
            },
            monthly: WindowCaps::default(),
        },
    };
    assert_eq!(
        set_budget(&fx.db, &negative).await.unwrap_err().reason(),
        crate::ErrorReason::InvalidArgument
    );
}

#[tokio::test]
async fn spend_report_separates_the_classes_and_says_which_caps_are_stored() {
    let fx = Fixture::open().await;
    let limits = open_limits();
    set_budget(
        &fx.db,
        &daily_usd_budget(GLOBAL_ACCOUNT_ID, BudgetClass::All, 1_000_000, 9_000_000),
    )
    .await
    .unwrap();
    fx.spend(now_ts(), None, 2.0, 100, WorkClass::Interactive)
        .await;
    fx.spend(now_ts(), None, 1.0, 50, WorkClass::Bulk).await;
    fx.spend(days_before(1), None, 4.0, 25, WorkClass::Bulk)
        .await;

    let report = spend_report(&fx.db, &limits, GLOBAL_ACCOUNT_ID, now_ts())
        .await
        .unwrap();
    assert_eq!(report.day, "2026-08-05");
    assert_eq!(report.month, "2026-08");
    assert_eq!(report.all.spend.daily.usd_micros, 3_000_000);
    assert_eq!(report.all.spend.daily.tokens, 150);
    assert_eq!(report.all.spend.monthly.usd_micros, 7_000_000);
    assert_eq!(report.bulk.spend.daily.usd_micros, 1_000_000);
    assert_eq!(report.bulk.spend.monthly.usd_micros, 5_000_000);
    assert!(report.all.stored, "an operator set the `all` budget");
    assert!(
        !report.bulk.stored,
        "the bulk sub-budget is derived, not stored, and the report must say so"
    );
    assert_eq!(
        report.bulk.caps.daily.hard_usd_micros,
        Some(4_500_000),
        "the derived bulk cap is `bulk_share` of the stored `all` hard cap"
    );
}

#[tokio::test]
async fn a_disabled_enforcer_allows_everything() {
    let fx = Fixture::open().await;
    let mut limits = open_limits();
    limits.budget.enabled = false;
    set_budget(
        &fx.db,
        &Budget {
            account_id: GLOBAL_ACCOUNT_ID,
            class: BudgetClass::All,
            caps: BudgetCaps {
                daily: WindowCaps {
                    hard_usd_micros: Some(1),
                    ..WindowCaps::default()
                },
                monthly: WindowCaps::default(),
            },
        },
    )
    .await
    .unwrap();
    fx.spend(now_ts(), None, 500.0, 0, WorkClass::Interactive)
        .await;

    assert_eq!(
        evaluate(
            &fx,
            &limits,
            fx.account_id,
            "claude-opus-4-8",
            WorkClass::Interactive
        )
        .await,
        BudgetVerdict::Allow
    );
}

#[tokio::test]
async fn set_budget_rejects_a_budget_that_caps_nothing() {
    let fx = Fixture::open().await;
    // A stored row wins over the configured fallback, so an all-unset one
    // would delete the `ai.limits` ceiling rather than leave it in place.
    let empty = Budget {
        account_id: GLOBAL_ACCOUNT_ID,
        class: BudgetClass::All,
        caps: BudgetCaps::default(),
    };
    assert_eq!(
        set_budget(&fx.db, &empty).await.unwrap_err().reason(),
        crate::ErrorReason::InvalidArgument
    );
    assert_eq!(
        get_budget(&fx.db, GLOBAL_ACCOUNT_ID, BudgetClass::All)
            .await
            .unwrap(),
        None,
        "the rejected budget must not have been stored"
    );

    // And the configured ceiling still applies afterwards.
    let limits = AiLimits {
        daily_token_cap: u64::MAX,
        daily_cost_cap_usd: 1.0,
        monthly_cost_cap_usd: 1_000_000.0,
        ..AiLimits::default()
    };
    fx.spend(now_ts(), None, 2.0, 0, WorkClass::Interactive)
        .await;
    assert!(matches!(
        evaluate(
            &fx,
            &limits,
            fx.account_id,
            "claude-haiku-4-5",
            WorkClass::Interactive
        )
        .await,
        BudgetVerdict::Block { .. }
    ));
}

#[tokio::test]
async fn a_bulk_share_of_zero_stops_bulk_work_rather_than_unleashing_it() {
    let fx = Fixture::open().await;
    // The dangerous direction: an operator setting this to 0 to stop backlog
    // spend must not be handed the *entire* parent budget because the value
    // failed a range check.
    let mut limits = AiLimits {
        daily_token_cap: u64::MAX,
        daily_cost_cap_usd: 100.0,
        monthly_cost_cap_usd: 1_000_000.0,
        ..AiLimits::default()
    };
    limits.budget.bulk_share = 0.0;

    assert!(
        matches!(
            evaluate(
                &fx,
                &limits,
                fx.account_id,
                "claude-haiku-4-5",
                WorkClass::Bulk
            )
            .await,
            BudgetVerdict::Block { .. }
        ),
        "bulk_share = 0 must block bulk work even with nothing spent yet"
    );
    assert_eq!(
        evaluate(
            &fx,
            &limits,
            fx.account_id,
            "claude-haiku-4-5",
            WorkClass::Interactive
        )
        .await,
        BudgetVerdict::Allow,
        "...and must not touch interactive work"
    );
}

#[tokio::test]
async fn a_soft_cap_will_not_downgrade_onto_a_model_the_ledger_cannot_price() {
    let fx = Fixture::open().await;
    let mut limits = open_limits();
    // A ladder pointing at a model `estimate_cost_usd` does not know. A
    // downgrade to it would record cost 0 for every later call, so the hard
    // cap would stop advancing exactly when it matters.
    limits.budget.ladder.sonnet = "some-unpriced-model".to_owned();
    set_budget(
        &fx.db,
        &daily_usd_budget(GLOBAL_ACCOUNT_ID, BudgetClass::All, 1_000_000, 9_000_000),
    )
    .await
    .unwrap();
    fx.spend(now_ts(), None, 5.0, 0, WorkClass::Interactive)
        .await;

    assert_eq!(
        evaluate(
            &fx,
            &limits,
            fx.account_id,
            "claude-opus-4-8",
            WorkClass::Interactive
        )
        .await,
        BudgetVerdict::Allow,
        "the requested (priceable) model must be kept rather than downgraded to one that would \
         make the hard cap unreachable"
    );
}

#[tokio::test]
async fn a_block_names_the_moment_its_window_rolls_over() {
    let fx = Fixture::open().await;
    let limits = open_limits();
    set_budget(
        &fx.db,
        &Budget {
            account_id: GLOBAL_ACCOUNT_ID,
            class: BudgetClass::All,
            caps: BudgetCaps {
                daily: WindowCaps {
                    hard_usd_micros: Some(1_000_000),
                    ..WindowCaps::default()
                },
                monthly: WindowCaps::default(),
            },
        },
    )
    .await
    .unwrap();
    fx.spend(now_ts(), None, 2.0, 0, WorkClass::Interactive)
        .await;

    let verdict = evaluate(
        &fx,
        &limits,
        fx.account_id,
        "claude-haiku-4-5",
        WorkClass::Interactive,
    )
    .await;
    let BudgetVerdict::Block { retry_at, .. } = verdict else {
        unreachable!("the daily cap is exhausted");
    };
    // 2026-08-06T00:00:00Z — the start of the next UTC day, which is when a
    // *daily* cap stops applying. A caller deferring a job to this instant
    // must not have it re-leased before then.
    let expected = chrono::Utc
        .with_ymd_and_hms(2026, 8, 6, 0, 0, 0)
        .single()
        .expect("2026-08-06T00:00:00Z is a real instant")
        .timestamp();
    assert_eq!(retry_at, expected);
}

#[test]
fn dollar_caps_round_trip_exactly_through_micro_dollars() {
    // The whole reason caps are integers: `5.00` is not representable in
    // binary floating point, so a float comparison at the boundary can land
    // on either side of it depending on how the sum accumulated.
    for usd in [0.0, 0.01, 5.00, 100.00, 1234.56] {
        let micros = usd_to_micros(usd);
        assert!(
            (micros_to_usd(micros) - usd).abs() < f64::EPSILON * 1024.0,
            "{usd} did not round-trip"
        );
    }
    assert_eq!(usd_to_micros(5.00), 5_000_000);
    assert_eq!(usd_to_micros(0.000_001), 1);
    assert_eq!(
        usd_to_micros(f64::INFINITY),
        i64::MAX,
        "an unrepresentable cap saturates rather than wrapping into a tiny one"
    );
}

// ---------------------------------------------------------------------------
// The property that actually matters: a blocked call never reaches the
// provider. Driven through the real `AiWorkerPool`, counting calls into the
// provider rather than inspecting an error after the fact — an assertion on
// the returned error would pass just as happily if the call had been made
// and its result thrown away.
// ---------------------------------------------------------------------------

/// Counts every `complete` call. Fails the test if one ever happens under a
/// hard cap.
#[derive(Debug, Default)]
struct CountingProvider {
    calls: Arc<AtomicUsize>,
    model_seen: Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait]
impl Provider for CountingProvider {
    async fn complete(
        &self,
        request: &ChatRequest,
        _cancel: &CancellationToken,
    ) -> Result<ChatResponse, Error> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.model_seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(request.model.clone());
        Ok(ChatResponse {
            id: "msg_test".to_owned(),
            model: request.model.clone(),
            text: "ok".to_owned(),
            usage: Usage {
                input_tokens: 1,
                output_tokens: 1,
                ..Usage::default()
            },
            stop_reason: StopReason::EndTurn,
        })
    }

    async fn stream(
        &self,
        _request: &ChatRequest,
        _cancel: &CancellationToken,
    ) -> Result<ProviderStream, Error> {
        Err(Error::internal("streaming is not used by these tests"))
    }
}

/// A pass that always asks for opus and persists nothing.
#[derive(Debug)]
struct OpusHandler;

#[async_trait]
impl PassHandler for OpusHandler {
    fn pass(&self) -> &str {
        "triage"
    }

    async fn build_request(&self, content: &MessageContent) -> Result<ChatRequest, Error> {
        Ok(ChatRequest {
            model: AiModels::default().deep,
            system: None,
            messages: vec![ChatMessage {
                role: Role::User,
                content: content.body.clone(),
            }],
            max_tokens: 64,
            output_format: None,
        })
    }

    async fn on_success(
        &self,
        _lease: &AiLease,
        _text: &str,
        _ledger_entry_id: i64,
    ) -> Result<(), Error> {
        Ok(())
    }
}

fn dispatch_pool(fx: &Fixture, provider: Arc<CountingProvider>, limits: AiLimits) -> AiWorkerPool {
    let provider: Arc<dyn Provider> = provider;
    AiWorkerPool::new(
        fx.db.clone(),
        AiQueue::new(fx.db.clone(), QueueOptions::default()),
        provider,
        Arc::new(PolicyEngine::new(Vec::new(), AiPolicyMode::Allowed, "unspecified").unwrap()),
        limits,
        AiPrivacy::default(),
        vec![Arc::new(OpusHandler) as Arc<dyn PassHandler>],
        "budget-test-worker",
        EventLog::new(fx.db.clone(), Retention::unlimited()),
    )
}

/// `ai.limits` with pacing wide open so a dispatch cycle is not slowed by the
/// RPM limiter, and the cycle-level `CostGate` far out of the way so only the
/// per-call enforcer can decide anything.
fn dispatch_limits() -> AiLimits {
    AiLimits {
        max_concurrency: 4,
        requests_per_minute: 1_000_000,
        ..open_limits()
    }
}

#[tokio::test]
async fn a_hard_cap_stops_dispatch_without_the_provider_being_called_at_all() {
    let fx = Fixture::open().await;
    let provider = Arc::new(CountingProvider::default());
    let queue = AiQueue::new(fx.db.clone(), QueueOptions::default());
    let message_id = fx.message().await;
    queue
        .enqueue(vec![NewAiJob::new(message_id, fx.account_id, "triage")])
        .await
        .unwrap();

    // The global daily hard cap is already spent.
    set_budget(
        &fx.db,
        &Budget {
            account_id: GLOBAL_ACCOUNT_ID,
            class: BudgetClass::All,
            caps: BudgetCaps {
                daily: WindowCaps {
                    hard_usd_micros: Some(1_000_000),
                    ..WindowCaps::default()
                },
                monthly: WindowCaps::default(),
            },
        },
    )
    .await
    .unwrap();
    fx.spend(
        chrono::Utc::now().timestamp(),
        None,
        2.0,
        0,
        WorkClass::Interactive,
    )
    .await;

    let pool = dispatch_pool(&fx, Arc::clone(&provider), dispatch_limits());
    let summary = pool
        .dispatch_pending(10, &CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        0,
        "a hard cap must mean the provider is never called, not that its answer was discarded"
    );
    assert_eq!(summary.withheld, 1);
    assert_eq!(summary.completed, 0);
    assert_eq!(summary.dead, 0, "a withheld job must not be quarantined");
    assert_eq!(
        summary.terminated, 0,
        "a withheld job must not be terminated: the window rolls over and the work is still wanted"
    );

    // Back in `pending`, attempt refunded, but held out of the candidate set
    // until the window rolls over rather than immediately re-leasable.
    let stats = queue.stats().await.unwrap();
    assert_eq!(
        stats.ready, 0,
        "a withheld job must not be leased again this window"
    );
    assert_eq!(
        stats.backing_off, 1,
        "the job is still pending — deferred, not lost, failed, or terminated"
    );
    let (attempts, next_attempt_at): (i64, i64) = fx
        .db
        .read(|conn| {
            conn.query_row(
                "SELECT attempts, next_attempt_at FROM ai_queue",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
        })
        .await
        .unwrap();
    assert_eq!(
        attempts, 0,
        "the attempt the lease charged must be handed back — a week at the cap must not \
         quarantine work that was never tried"
    );
    assert!(
        next_attempt_at > chrono::Utc::now().timestamp(),
        "the job must be deferred past now, or every tick would re-lease it and starve \
         uncapped accounts queued behind it"
    );

    // ...and a second cycle must not touch it either, which is the property
    // a plain `release` would have failed.
    let again = pool
        .dispatch_pending(10, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(again.withheld, 0, "the deferred job is not re-leased");
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_soft_cap_dispatches_the_downgraded_model_to_the_provider() {
    let fx = Fixture::open().await;
    let provider = Arc::new(CountingProvider::default());
    let queue = AiQueue::new(fx.db.clone(), QueueOptions::default());
    let message_id = fx.message().await;
    queue
        .enqueue(vec![NewAiJob::new(message_id, fx.account_id, "triage")])
        .await
        .unwrap();

    set_budget(
        &fx.db,
        &daily_usd_budget(GLOBAL_ACCOUNT_ID, BudgetClass::All, 1_000_000, 90_000_000),
    )
    .await
    .unwrap();
    fx.spend(
        chrono::Utc::now().timestamp(),
        None,
        2.0,
        0,
        WorkClass::Interactive,
    )
    .await;

    let limits = dispatch_limits();
    let expected = limits.budget.ladder.sonnet.clone();
    let pool = dispatch_pool(&fx, Arc::clone(&provider), limits);
    let summary = pool
        .dispatch_pending(10, &CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(summary.completed, 1);
    assert_eq!(
        summary.withheld, 0,
        "a soft cap downgrades, it does not block"
    );
    let models = provider
        .model_seen
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(
        models,
        vec![expected],
        "the provider must have been asked for the downgraded model, not the one the handler \
         originally chose"
    );
}

#[tokio::test]
async fn a_backfill_job_is_charged_to_the_bulk_sub_budget_on_the_dispatch_path() {
    let fx = Fixture::open().await;
    let provider = Arc::new(CountingProvider::default());
    let queue = AiQueue::new(fx.db.clone(), QueueOptions::default());
    let message_id = fx.message().await;
    queue
        .enqueue(vec![
            NewAiJob::new(message_id, fx.account_id, "triage").priority(PRIORITY_BACKFILL)
        ])
        .await
        .unwrap();

    let pool = dispatch_pool(&fx, Arc::clone(&provider), dispatch_limits());
    let summary = pool
        .dispatch_pending(10, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(summary.completed, 1);

    assert_eq!(
        fx.only_work_class().await,
        "bulk",
        "a job enqueued at PRIORITY_BACKFILL must be charged to the bulk sub-budget, or the \
         sub-budget can never be enforced"
    );
}
