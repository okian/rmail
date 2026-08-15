//! The acceptance of task 54: the tool set prd.md promises, the listing a
//! given connection gets, and the claim that makes the whole projection worth
//! building — that an agent's `search_mail` *is* the human's search.
//!
//! # Why the daemon-backed tests are here rather than in `rmaild/tests/`
//!
//! Task 53 put its socket-needing tests in `rmaild/tests/mcp_server.rs`, which
//! is the house pattern. Task 54's `verify` line is
//! `cargo nextest run -p rmaild mcp::tools`, and nextest matches a bare
//! positional filter against a test's *name*, not against its binary id (the
//! same trap `rmaild/tests/ask_mailbox.rs` documents). A suite in
//! `tests/mcp_tools.rs` would therefore be selected by that command not at
//! all, and the acceptance bullet it covers — search parity with
//! `SearchService` — would be the one nobody ran. So the tests that need a
//! daemon live in this module, where their names begin `mcp::tools::tests::`.
//!
//! # What prd.md names that nothing projects to
//!
//! prd.md's "MCP Tools (search)" section also promises `similar_messages`
//! (`{message_uid, k}` -> nearest neighbours). No RPC serves it today and none
//! is in the descriptor set, so there is nothing here to project and nothing
//! to assert — it belongs to whichever task builds the RPC. It is named here
//! rather than silently omitted so the gap is a known one.

use std::cell::Cell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use rmail_core::index::fts::FtsIndex;
use rmail_core::index::{extract_message, IndexQueue, QueueOptions, PRIORITY_NORMAL};
use rmail_core::parity::Command;
use rmail_core::{repo, Config, Database};
use rmail_proto::v1::search_service_client::SearchServiceClient;
use rmail_proto::v1::{SearchHit, SearchRequest};
use serde_json::{json, Value};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_stream::StreamExt as _;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::mcp::projection::tests::MUTATIONS_A_READ_TOKEN_REACHES;
use crate::mcp::{CallLimits, McpServer, Principal};

// ---------------------------------------------------------------------------
// The tool set prd.md promises
// ---------------------------------------------------------------------------

/// One capability prd.md promises an agent, and the registry row that serves
/// it.
struct Promised {
    /// The registry row — a typed [`Command`], not a string, so this stays a
    /// check that the *capability* is projected. A rename of the row's `tool:`
    /// still compiles here and is caught by `projected` below, which is the
    /// place a name change should fail.
    command: Command,
    /// What prd.md calls it in prose.
    prd: &'static str,
    /// What the parity registry names it, which is what an MCP client sees.
    projected: &'static str,
}

/// The agent loop prd.md describes — find mail, read it, understand it — as
/// registry rows.
///
/// Two entries diverge from prd.md's own prose, and the divergence is recorded
/// rather than papered over (`the_prd_core_tool_set_is_projected` asserts
/// prd.md's spelling really is absent, so adding a tool under it forces this
/// table to be updated instead of leaving a stale note):
///
/// - `read_mail` — prd.md's III-1 section chains `fuzzy_find` into
///   "`read_mail`/`archive_mail`/`draft_reply`". The registry spells the
///   capability `get_message`, matching its siblings `get_thread`,
///   `get_summary` and `get_attachment`. Same RPC, same authority.
/// - `summarize_thread` — prd.md's feature 1 names an
///   `AiService.SummarizeThread` streaming RPC. What exists is
///   `AiService/GetSummary`, whose `Summary` carries `thread_summary` (the
///   rollup prd.md's III-2 section describes), plus `AiService/AnalyzeMessage`
///   for forcing a fresh deep pass. The capability is served; the RPC prd.md
///   sketched was not built under that name.
const PRD_CORE_TOOLS: &[Promised] = &[
    Promised {
        command: Command::SearchSearch,
        prd: "search_mail",
        projected: "search_mail",
    },
    Promised {
        command: Command::SearchSemantic,
        prd: "semantic_search",
        projected: "semantic_search",
    },
    Promised {
        command: Command::SearchExplain,
        prd: "explain_ranking",
        projected: "explain_ranking",
    },
    Promised {
        command: Command::FinderFind,
        prd: "fuzzy_find",
        projected: "fuzzy_find",
    },
    Promised {
        command: Command::MailList,
        prd: "list_messages",
        projected: "list_messages",
    },
    Promised {
        command: Command::MailGet,
        prd: "read_mail",
        projected: "get_message",
    },
    Promised {
        command: Command::MailGetThread,
        prd: "get_thread",
        projected: "get_thread",
    },
    Promised {
        command: Command::MailGetAttachment,
        prd: "get_attachment",
        projected: "get_attachment",
    },
    Promised {
        command: Command::AiGetSummary,
        prd: "summarize_thread",
        projected: "get_summary",
    },
    Promised {
        command: Command::AiAnalyzeMessage,
        prd: "analyze_message",
        projected: "analyze_message",
    },
    Promised {
        command: Command::AiAskMailbox,
        prd: "ask_mailbox",
        projected: "ask_mailbox",
    },
];

fn surface() -> ToolSurface {
    ToolSurface::build().expect("the surface must project")
}

/// The acceptance's "the PRD's core tool set is present", checked against the
/// registry rather than against a second roster of names.
#[test]
fn the_prd_core_tool_set_is_projected() {
    let surface = surface();
    for promised in PRD_CORE_TOOLS {
        let Some(tool) = surface
            .tools()
            .iter()
            .find(|tool| tool.command() == promised.command)
        else {
            unreachable!(
                "prd.md promises {:?} ({:?}), which nothing in the projected surface serves",
                promised.prd, promised.command
            )
        };
        assert_eq!(
            tool.name(),
            promised.projected,
            "{:?} projects under a name this table does not record",
            promised.command
        );
        if promised.prd != promised.projected {
            assert!(
                surface.get(promised.prd).is_none(),
                "a tool now exists called {:?}, so PRD_CORE_TOOLS' note about {} projecting under \
                 {:?} instead is stale",
                promised.prd,
                promised.projected,
                promised.projected
            );
        }
    }
}

/// Every promised tool arrives with a schema a model can fill in, or "present"
/// means nothing: a tool with no usable `inputSchema` is one an MCP client
/// will not call.
#[test]
fn every_promised_tool_carries_a_usable_input_schema() {
    let surface = surface();
    for promised in PRD_CORE_TOOLS {
        let tool = surface
            .get(promised.projected)
            .expect("checked present above");
        let json = tool.to_json();
        assert_eq!(json["inputSchema"]["type"], "object", "{}", promised.prd);
        assert!(
            json["inputSchema"]["properties"].is_object(),
            "{} has no properties map",
            promised.prd
        );
        assert!(
            json["description"]
                .as_str()
                .is_some_and(|text| text.contains("Requires ")),
            "{} does not tell the agent what scope it needs",
            promised.prd
        );
    }
}

// ---------------------------------------------------------------------------
// The listing a connection gets
// ---------------------------------------------------------------------------

fn read_only_scopes() -> Vec<Scope> {
    vec![Scope::MailRead]
}

/// The default policy lists what the daemon would accept — including the one
/// mutating tool a `mail.read` token genuinely reaches.
///
/// This is the *negative* half of task 54's decision, and it is the one worth
/// pinning: the listing must not quietly shrink to look tidy. The expected set
/// is `projection::tests`' own, not a copy, so the two cannot drift.
#[test]
fn the_default_listing_still_offers_the_mutations_a_read_token_reaches() {
    let surface = surface();
    let visibility = Visibility::scoped(read_only_scopes());
    let mut reachable: Vec<&str> = visibility
        .list(&surface)
        .filter(|tool| tool.effect() == Effect::Mutate)
        .map(Tool::name)
        .collect();
    reachable.sort_unstable();
    assert_eq!(
        reachable, MUTATIONS_A_READ_TOKEN_REACHES,
        "the default listing must be exactly what the daemon will accept; hiding a mutation the \
         daemon would still run tells the agent it cannot act while the daemon disagrees"
    );
}

/// The acceptance's sentence, made true by the policy that also *refuses*
/// what it hides.
#[test]
fn a_read_only_surface_lists_only_read_tools() {
    let surface = surface();
    let visibility = Visibility::new(read_only_scopes(), Mutations::Withheld);
    let listed: Vec<&Tool> = visibility.list(&surface).collect();

    assert!(
        !listed.is_empty(),
        "a read-only surface must still offer something"
    );
    for tool in &listed {
        assert_eq!(
            tool.effect(),
            Effect::Read,
            "{} changes state and must not appear in a read-only listing",
            tool.name()
        );
        assert_eq!(
            tool.to_json()["annotations"]["readOnlyHint"],
            Value::Bool(true),
            "{} is listed read-only and must be annotated as such",
            tool.name()
        );
    }
    // ...and the exception the default listing keeps is exactly what this one
    // drops, rather than the two policies happening to agree.
    for name in MUTATIONS_A_READ_TOKEN_REACHES {
        assert!(
            !listed.iter().any(|tool| tool.name() == *name),
            "{name} mutates at mail.read and must be withheld from a read-only surface"
        );
    }
}

/// **The property that makes withholding honest rather than a shorter lie.**
///
/// A listing may omit a tool only if this process will also refuse to send it.
/// Filtering the listing without filtering the gate is the failure task 54 was
/// warned about from the other direction: the agent would be told less than it
/// can do, and would find out only by guessing a name.
///
/// Checked over the whole surface and every policy this type can express, not
/// on a sample — one tool listed but refused, or refused but listed, is one an
/// agent trips over.
#[test]
fn nothing_withheld_from_the_listing_is_still_callable() {
    let surface = surface();
    for mutations in [Mutations::AsScoped, Mutations::Withheld] {
        for granted in [
            vec![],
            read_only_scopes(),
            vec![Scope::MailRead, Scope::AiInvoke],
            vec![Scope::MailWrite],
            vec![Scope::Admin],
        ] {
            let visibility = Visibility::new(granted.clone(), mutations);
            let listed: Vec<&str> = visibility.list(&surface).map(Tool::name).collect();
            for tool in surface.tools() {
                let authorized = visibility.authorize(&surface, tool.name()).is_ok();
                assert_eq!(
                    listed.contains(&tool.name()),
                    authorized,
                    "{} is listed={} but callable={} under {:?} with {:?}; the listing and the \
                     gate must describe one surface",
                    tool.name(),
                    listed.contains(&tool.name()),
                    authorized,
                    mutations,
                    granted
                );
            }
        }
    }
}

/// Withholding may only ever take tools away. A policy that *added* one would
/// be a widening dressed as a restriction.
#[test]
fn withholding_mutations_only_ever_removes_tools() {
    let surface = surface();
    for granted in [
        read_only_scopes(),
        vec![Scope::MailWrite],
        vec![Scope::Admin],
    ] {
        let wide: Vec<&str> = Visibility::scoped(granted.clone())
            .list(&surface)
            .map(Tool::name)
            .collect();
        let narrow: Vec<&str> = Visibility::new(granted.clone(), Mutations::Withheld)
            .list(&surface)
            .map(Tool::name)
            .collect();
        for name in &narrow {
            assert!(
                wide.contains(name),
                "{name} appears only under Withheld with {granted:?}, which would make a \
                 restriction into a widening"
            );
        }
        assert!(
            narrow.len() < wide.len(),
            "withholding removed nothing with {granted:?}; the surface has no mutating tools at \
             all, which cannot be right"
        );
    }
}

/// Even admin — which satisfies every scope — is bound by it. A policy the
/// widest token escapes is not a policy.
#[test]
fn an_admin_connection_is_still_bound_by_a_read_only_surface() {
    let surface = surface();
    let visibility = Visibility::new(vec![Scope::Admin], Mutations::Withheld);
    for tool in visibility.list(&surface) {
        assert_eq!(tool.effect(), Effect::Read, "{}", tool.name());
    }
    assert!(visibility.authorize(&surface, "delete_message").is_err());
}

/// A read-only surface is still worth handing an agent: the whole find →
/// read → understand loop survives it, minus the parts that spend or write.
#[test]
fn a_read_only_surface_still_serves_the_prd_read_loop() {
    let surface = surface();
    let visibility = Visibility::new(vec![Scope::MailRead, Scope::AiInvoke], Mutations::Withheld);
    let listed: Vec<&str> = visibility.list(&surface).map(Tool::name).collect();
    for name in [
        "search_mail",
        "semantic_search",
        "explain_ranking",
        "fuzzy_find",
        "list_messages",
        "get_message",
        "get_thread",
        "get_attachment",
        "get_summary",
    ] {
        assert!(
            listed.contains(&name),
            "{name} observes and must survive a read-only surface: {listed:?}"
        );
    }
    // `ask_mailbox` and `analyze_message` do not, and that is the trade being
    // made rather than an oversight: both spend at a model provider, which
    // `Effect`'s own docs count as an effect an observer outside this process
    // can see.
    for name in ["ask_mailbox", "analyze_message"] {
        assert!(
            !listed.contains(&name),
            "{name} spends at a provider and must not survive a read-only surface"
        );
    }
}

/// A withheld tool is refused *by reason*, never reported as missing.
#[test]
fn a_withheld_tool_is_refused_by_reason_rather_than_reported_missing() {
    let surface = surface();
    let visibility = Visibility::new(read_only_scopes(), Mutations::Withheld);

    let error = visibility
        .authorize(&surface, "log_search_feedback")
        .expect_err("a read-only surface must refuse a mutating tool");
    let McpError::Withheld {
        tool,
        scope_shortfall,
    } = &error
    else {
        unreachable!("expected a withholding, got {error:?}")
    };
    assert_eq!(tool, "log_search_feedback");
    assert!(
        scope_shortfall.is_empty(),
        "mail.read reaches this tool, so lifting --read-only is the whole fix: {scope_shortfall}"
    );
    assert_ne!(
        error.code(),
        -32601,
        "a withheld tool must not look like a missing one, or the agent re-reads the list forever"
    );
    let text = error.to_string();
    assert!(text.contains("read-only"), "{text}");

    // A genuinely unknown name still resolves to the unknown-tool error, so
    // the two remain distinguishable.
    let unknown = visibility
        .authorize(&surface, "no_such_tool")
        .expect_err("an unknown tool must not resolve");
    assert!(matches!(unknown, McpError::UnknownTool(_)), "{unknown:?}");

    // ...and the same tool under the default policy is allowed, which is what
    // makes the refusal above this process's own choice rather than a scope.
    assert!(Visibility::scoped(read_only_scopes())
        .authorize(&surface, "log_search_feedback")
        .is_ok());
}

/// A refusal names every constraint that binds, not just the first one hit.
///
/// The trap this pins: a `mail.read` connection to a read-only server asking
/// for `delete_message` fails *both* rules. Reporting only the withholding
/// sends an operator to restart without `--read-only` and straight into a
/// scope denial on the next call; reporting only the scope sends them to mint
/// a token this server would refuse anyway. Either alone costs a whole
/// human round trip to learn the other half.
#[test]
fn a_refusal_names_every_constraint_that_binds() {
    let surface = surface();

    // Scope alone.
    let denied = Visibility::scoped(read_only_scopes())
        .authorize(&surface, "delete_message")
        .expect_err("mail.read must not reach a delete");
    let McpError::Denied { requires, .. } = &denied else {
        unreachable!("expected a scope denial, got {denied:?}")
    };
    assert!(requires.contains("mail.write"), "{requires}");

    // Both. Still a withholding — that is the constraint this process will not
    // relax — but the message has to carry the scope too.
    let both = Visibility::new(read_only_scopes(), Mutations::Withheld)
        .authorize(&surface, "delete_message")
        .expect_err("a read-only surface must refuse a delete");
    let McpError::Withheld {
        scope_shortfall, ..
    } = &both
    else {
        unreachable!("expected a withholding, got {both:?}")
    };
    assert!(
        scope_shortfall.contains("mail.write"),
        "the refusal must name the scope that also binds: {scope_shortfall}"
    );
    let text = both.to_string();
    assert!(text.contains("read-only"), "{text}");
    assert!(text.contains("mail.write"), "{text}");

    // The effect rule alone, for a token that *does* reach the tool: nothing
    // about scopes, because there is nothing to say.
    let effect_only = Visibility::new(vec![Scope::Admin], Mutations::Withheld)
        .authorize(&surface, "delete_message")
        .expect_err("a read-only surface must refuse a delete even to admin");
    assert!(
        !effect_only.to_string().contains("does not hold"),
        "admin holds everything; naming a scope shortfall here would be wrong: {effect_only}"
    );
}

// ---------------------------------------------------------------------------
// Against a running daemon
// ---------------------------------------------------------------------------

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// How long one `tools/call` may take before the test gives up.
///
/// Every call here is already bounded by [`CallLimits::timeout`], so this only
/// fires when that bound has regressed — which is a hang rather than a wrong
/// answer, and an unbounded await would wedge the suite instead of reporting
/// it. Task 53 learned this the expensive way (see `rmaild/tests/mcp_server.rs`'s
/// `CALL_PATIENCE`).
const CALL_PATIENCE: Duration = Duration::from_secs(45);

/// A daemon over a throwaway database, with the search pipeline wired exactly
/// as `rmaild` boots it.
struct Daemon {
    socket: PathBuf,
    db_path: PathBuf,
    db: Database,
    fts: FtsIndex,
    queue: IndexQueue,
    next_uid: Cell<i64>,
    shutdown: Option<oneshot::Sender<()>>,
    handle: Option<JoinHandle<Result<(), crate::ServeError>>>,
}

impl Daemon {
    async fn start() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        // `/tmp` rather than `temp_dir()` for the socket: macOS caps
        // `sockaddr_un` at 104 bytes and the default temp dir eats most of it.
        let socket = PathBuf::from("/tmp").join(format!("rmail-mcptools-{pid}-{n}.sock"));
        let db_path = std::env::temp_dir().join(format!("rmail-mcptools-{pid}-{n}.db"));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", db_path.display())));
        }
        let db = Database::open(&db_path).expect("open a database");
        db.with_write(move |c| {
            let account_id = repo::insert_account(
                c,
                &repo::NewAccount {
                    name: format!("Personal-{n}"),
                    ..Default::default()
                },
            )?;
            repo::insert_mailbox(
                c,
                &repo::NewMailbox {
                    account_id,
                    name: "INBOX".to_owned(),
                    ..Default::default()
                },
            )
        })
        .expect("seed an account and a mailbox");

        // Semantic indexing off, the convention `serve_uds` itself follows: an
        // enabled default would make this test load — or on a cold cache
        // download — an ONNX model purely to exercise the lexical path.
        let mut config = Config::default();
        config.index.semantic.enabled = false;
        // AI off. `every_promised_tool_dispatches_against_a_running_daemon`
        // really calls `ask_mailbox`, and the default `api_key_command` is a
        // macOS keychain lookup — on a developer's own machine that would pop
        // a keychain prompt and, on a hit, make a live billed request with an
        // empty question. Disabled, the RPC declines at the policy gate, which
        // is still a dispatch and still what that test asserts.
        config.ai.enabled = false;
        // Learning off, so the ranked-set comparison cannot be coupled to what
        // the *first* of its two searches wrote. Nothing reads the feedback
        // log back at query time today (`search_service`'s own
        // `no_search_rpc_reads_the_feedback_log_back_out`), so this removes a
        // latent dependency rather than an actual one.
        config.search.learning = false;
        let fts = FtsIndex::new(db.clone(), config.search.bm25_weights.clone());
        let queue = IndexQueue::new(db.clone(), QueueOptions::default());

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let server_socket = socket.clone();
        let server_db = db.clone();
        let handle = tokio::spawn(async move {
            crate::serve_uds_with_config(&server_socket, server_db, config, async move {
                let _ = shutdown_rx.await;
            })
            .await
        });

        let mut ready = false;
        for _ in 0..300 {
            if rmail_core::connect_uds(&socket).await.is_ok() {
                ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(ready, "the daemon never became ready");

        Self {
            socket,
            db_path,
            db,
            fts,
            queue,
            next_uid: Cell::new(1),
            shutdown: Some(shutdown_tx),
            handle: Some(handle),
        }
    }

    /// Insert, extract and lexically index one message — the real pipeline,
    /// mirroring `rmaild/tests/search_service.rs`'s own seeding.
    async fn index(&self, subject: &str, body: &str, date: i64) -> i64 {
        let uid = self.next_uid.get();
        self.next_uid.set(uid + 1);
        let new = repo::NewMessage {
            account_id: 1,
            mailbox_id: 1,
            uid,
            uidvalidity: 1,
            subject: Some(subject.to_owned()),
            body_text: Some(body.to_owned()),
            date: Some(date),
            ..Default::default()
        };
        let message_id = self
            .db
            .with_write(move |c| repo::insert_message(c, &new))
            .expect("insert a message");
        // Bounded like every other await in this file. Seeding is fixture work
        // rather than the thing under test, which is exactly why a stall here
        // would read as a slow suite instead of a broken one.
        let indexed = tokio::time::timeout(CALL_PATIENCE, async {
            extract_message(&self.db, &self.queue, message_id, PRIORITY_NORMAL)
                .await
                .expect("extract");
            self.fts.index_message(message_id).await.expect("index");
        })
        .await;
        assert!(
            indexed.is_ok(),
            "indexing message {message_id} did not finish within {CALL_PATIENCE:?}"
        );
        message_id
    }

    /// A corpus whose members match one term with clearly different strength,
    /// so "the same ranked order" is a claim about ranking rather than about
    /// two singleton lists.
    async fn seed_corpus(&self) -> Vec<i64> {
        let mut ids = Vec::new();
        for (offset, (subject, body)) in [
            (
                "Quarterly budgetary review",
                "The budgetary review covers every budgetary allocation for the quarter, and the \
                 budgetary committee signed it off.",
            ),
            (
                "Budgetary forecast",
                "Attached is the budgetary forecast for next year.",
            ),
            (
                "Notes from the offsite",
                "We touched on the budgetary question briefly before lunch.",
            ),
            (
                "Team lunch",
                "Lunch on Friday, with no budgetary implications whatsoever.",
            ),
        ]
        .into_iter()
        .enumerate()
        {
            ids.push(
                self.index(subject, body, 1_700_000_000 + offset as i64 * 3600)
                    .await,
            );
        }
        ids
    }

    async fn mcp(&self, scopes: Vec<Scope>, mutations: Mutations) -> McpServer {
        let channel = rmail_core::connect_uds(&self.socket)
            .await
            .expect("connect to the daemon");
        McpServer::new(
            channel,
            Principal {
                scopes,
                bearer: None,
                mutations,
            },
            CallLimits {
                max_frames: 32,
                timeout: Duration::from_secs(20),
            },
            CancellationToken::new(),
        )
        .expect("the surface must project")
    }

    async fn search_client(&self) -> SearchServiceClient<tonic::transport::Channel> {
        SearchServiceClient::new(
            rmail_core::connect_uds(&self.socket)
                .await
                .expect("connect to the daemon"),
        )
    }

    async fn stop(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.handle.take() {
            let _ = tokio::time::timeout(Duration::from_secs(10), handle).await;
        }
        let _ = std::fs::remove_file(&self.socket);
        for suffix in ["", "-wal", "-shm"] {
            let _ =
                std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.db_path.display())));
        }
    }
}

/// One JSON-RPC request, bounded so a regression in the call's own limits
/// reports as a failure rather than as a hang.
async fn ask(server: &McpServer, request: &Value) -> Value {
    let Ok(answer) = tokio::time::timeout(CALL_PATIENCE, server.handle(&request.to_string())).await
    else {
        unreachable!("no answer within {CALL_PATIENCE:?}; the call's own bounds are not binding")
    };
    let text = answer.expect("a request with an id must be answered");
    serde_json::from_str(&text).expect("the answer must be JSON")
}

async fn call(server: &McpServer, name: &str, arguments: Value) -> Value {
    let response = ask(
        server,
        &json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": name, "arguments": arguments },
        }),
    )
    .await;
    assert!(
        response.get("error").is_none(),
        "{name} failed at the protocol level: {response}"
    );
    response["result"].clone()
}

/// A hit as the two surfaces each describe it, minus the one field that
/// legitimately differs: `query_id` names the response, and two searches are
/// two responses.
type Row = (i64, f64, Vec<String>);

fn from_grpc(hit: &SearchHit) -> Row {
    (
        hit.message.as_ref().map_or(-1, |message| message.id),
        hit.score,
        hit.sources.clone(),
    )
}

fn from_mcp(frame: &Value) -> Row {
    (
        frame["message"]["id"].as_i64().unwrap_or(-1),
        frame["score"].as_f64().unwrap_or(f64::NAN),
        frame["sources"]
            .as_array()
            .map(|sources| {
                sources
                    .iter()
                    .filter_map(|source| source.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default(),
    )
}

/// **The acceptance's second bullet.** MCP `search_mail` returns the exact
/// ranked set `SearchService/Search` returns, because it *is* that call.
///
/// The one thing not asserted bit-for-bit is the score. Both requests run the
/// identical pipeline over the identical corpus, but `search_service` captures
/// `Utc::now()` per request and `FeatureExtractor::extract_at` decays recency
/// from it, so two searches milliseconds apart differ in the last few bits of
/// a `f64`. Demanding equality there would pin the clock rather than the
/// ranking. The order — which is what "the ranked set" means, and what an
/// agent acts on — is compared exactly.
#[tokio::test]
async fn search_mail_returns_the_same_ranked_set_as_the_search_service() {
    let daemon = Daemon::start().await;
    let seeded = daemon.seed_corpus().await;

    // The human's search: the generated client, straight onto the RPC.
    let mut client = daemon.search_client().await;
    let mut stream = client
        .search(SearchRequest {
            query: "budgetary".to_owned(),
            limit: 10,
            ..Default::default()
        })
        .await
        .expect("Search must be accepted")
        .into_inner();
    let mut human: Vec<Row> = Vec::new();
    loop {
        // Bounded rather than a bare `stream.next()`: a stream that stalls
        // would otherwise hang the suite, and a stalled stream that silently
        // ended the loop would look like a shorter — but agreeing — result.
        let Ok(next) = tokio::time::timeout(CALL_PATIENCE, stream.next()).await else {
            unreachable!("Search stalled after {} hits", human.len())
        };
        match next {
            None => break,
            Some(item) => human.push(from_grpc(&item.expect("a hit, not a status"))),
        }
    }

    assert!(
        human.len() >= 3,
        "the corpus must produce a ranked list worth comparing, got {}",
        human.len()
    );
    assert!(
        human.windows(2).any(|pair| pair[0].1 > pair[1].1),
        "every hit scored the same, so 'the same order' would prove nothing: {human:?}"
    );
    for (id, ..) in &human {
        assert!(
            seeded.contains(id),
            "{id} is not one of the seeded messages"
        );
    }

    // The agent's search: the same call, reached through the projection with
    // nothing per-RPC in between.
    let mcp = daemon.mcp(read_only_scopes(), Mutations::AsScoped).await;
    let result = call(
        &mcp,
        "search_mail",
        json!({ "query": "budgetary", "limit": 10 }),
    )
    .await;
    assert_eq!(result["isError"], false, "{result}");
    let frames = result["structuredContent"]["frames"]
        .as_array()
        .expect("a frame array")
        .clone();
    assert_eq!(
        result["structuredContent"]["truncated"], false,
        "the comparison is only meaningful over a complete answer: {result}"
    );
    let agent: Vec<Row> = frames.iter().map(from_mcp).collect();

    assert_eq!(
        agent.iter().map(|row| row.0).collect::<Vec<_>>(),
        human.iter().map(|row| row.0).collect::<Vec<_>>(),
        "the agent's ranked message ids must be the human's, in the same order"
    );
    for (a, h) in agent.iter().zip(&human) {
        assert_eq!(
            a.2, h.2,
            "message {} was surfaced by different retrievers",
            a.0
        );
        assert!(
            (a.1 - h.1).abs() < 1e-6,
            "message {} scored {} for the agent and {} for the human",
            a.0,
            a.1,
            h.1
        );
    }

    daemon.stop().await;
}

/// The acceptance's "and invocable": every promised tool dispatches — the
/// projection resolves the name, the schema accepts an argument object, the
/// encoded request reaches the daemon, and its answer decodes back.
///
/// The result may well be `isError` (an empty argument object names no
/// message, and `ask_mailbox` has no question to answer), which is the point:
/// what is asserted is that the call *ran*, rather than being refused by the
/// gate or rejected as malformed before it left the process.
#[tokio::test]
async fn every_promised_tool_dispatches_against_a_running_daemon() {
    let daemon = Daemon::start().await;
    let seeded = daemon.seed_corpus().await;
    let mcp = daemon.mcp(vec![Scope::Admin], Mutations::AsScoped).await;

    // "Not refused" asserted structurally rather than by grepping the refusal
    // text: the listing *is* the gate (`nothing_withheld_from_the_listing_is_
    // still_callable`), so a tool this connection lists is one it will send,
    // and a substring match on `McpError::Denied`'s wording would rot the
    // moment that sentence is reworded.
    let listed: Vec<&str> = mcp.visible_tools().iter().map(|tool| tool.name()).collect();
    for promised in PRD_CORE_TOOLS {
        assert!(
            listed.contains(&promised.projected),
            "{} is not offered to an admin connection, so calling it proves nothing",
            promised.projected
        );
        let result = call(&mcp, promised.projected, json!({})).await;
        assert!(
            result["isError"].is_boolean(),
            "{} produced no tool result at all: {result}",
            promised.projected
        );
        assert!(
            result["content"][0]["text"].is_string(),
            "{} answered with no content: {result}",
            promised.projected
        );
    }

    // ...and one of them with real arguments, so "dispatched" is not only
    // "reached a handler that said no".
    let read = call(&mcp, "get_message", json!({ "id": seeded[0] })).await;
    assert_eq!(read["isError"], false, "{read}");
    let detail = &read["structuredContent"];
    assert_eq!(detail["message"]["id"], seeded[0], "{read}");
    assert_eq!(
        detail["message"]["subject"], "Quarterly budgetary review",
        "{read}"
    );
    // The body too, so this is a message actually read rather than a stub with
    // the right id: the tool prd.md calls `read_mail` has to return the mail.
    assert!(
        detail["body_text"]
            .as_str()
            .is_some_and(|body| body.contains("budgetary committee")),
        "{read}"
    );

    daemon.stop().await;
}

/// The listing and the gate agree over a real connection, not only in the
/// constructed surface: what a read-only server omits, it also refuses.
#[tokio::test]
async fn a_read_only_connection_refuses_the_mutation_its_listing_omits() {
    let daemon = Daemon::start().await;
    let mcp = daemon.mcp(read_only_scopes(), Mutations::Withheld).await;

    let response = ask(
        &mcp,
        &json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
    )
    .await;
    let tools = response["result"]["tools"].as_array().expect("a tool list");
    assert!(!tools.is_empty());
    assert!(
        tools.iter().any(|tool| tool["name"] == "search_mail"),
        "a read-only surface must still offer search"
    );
    for tool in tools {
        assert_eq!(
            tool["annotations"]["readOnlyHint"],
            Value::Bool(true),
            "{tool} is listed by a read-only server and must be read-only"
        );
    }
    assert!(
        !tools
            .iter()
            .any(|tool| tool["name"] == "log_search_feedback"),
        "log_search_feedback mutates and must be withheld"
    );

    // The withheld tool is refused here rather than sent, so the shorter
    // listing is a true description of this connection.
    let refused = call(&mcp, "log_search_feedback", json!({ "query_id": "x" })).await;
    assert_eq!(refused["isError"], true, "{refused}");
    let text = refused["content"][0]["text"].as_str().unwrap_or_default();
    assert!(text.contains("read-only"), "{text}");
    // Not a JSON-RPC error: a refusal belongs in the model's context, where it
    // can reason about it, not in a client-side path it never sees.
    assert!(
        refused["content"][0]["type"] == "text",
        "the refusal must reach the model as content: {refused}"
    );

    // The instructions say so too, so an agent can explain the surface to its
    // human instead of inferring it from a short list.
    let initialized = ask(
        &mcp,
        &json!({ "jsonrpc": "2.0", "id": 2, "method": "initialize", "params": {} }),
    )
    .await;
    let instructions = initialized["result"]["instructions"]
        .as_str()
        .unwrap_or_default();
    assert!(instructions.contains("read-only"), "{instructions}");

    daemon.stop().await;
}
