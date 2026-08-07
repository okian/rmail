//! Compiling `QueryPlan::hard_filters` into one SQL predicate, shared by
//! every retriever in this task that is not [`super::lexical`].
//!
//! # Why this exists instead of six copies
//!
//! [`super::dense`], [`super::entity`], [`super::fuzzy`], [`super::prefix`],
//! [`super::recency`], and [`super::structured`] all need the same thing:
//! "only messages this query's `from:`/`is:`/`before:`/... constraints
//! allow". Writing that predicate once and reusing it is not just less code —
//! it is the difference between six retrievers that agree about what `-in:Spam`
//! means and six retrievers that each got one detail (NULL-safety on
//! negation, an unbacked `note:` filter excluding everything, a malformed
//! `thread:` id) slightly differently. [`compile`] is that one place — and
//! [`super::lexical`], which does *not* share it (see the next section), is
//! exactly the shape of gap this design closes for the other six: task 48's
//! own postmortem was a `tag:`-shaped fix landing in only one of `compile`/
//! `lexical::compile_filters`, silently dropping the BM25 arm from any query
//! mixing free text with the operator.
//!
//! # Why not share `retrieve::lexical`'s compiler instead
//!
//! `lexical.rs`'s `compile_filters` does almost this, and duplicating it here
//! is a real cost. It was not reused because it operates on a different
//! input: [`crate::query::Filter`], as task 25 produced it, with a
//! `before:`/`after:`/`on:`/`date:` value still a raw string — task 27 predates
//! task 26's `QueryPlan` in the dependency graph, so lexical.rs has never seen
//! a resolved date. This task's retrievers *do* — [`crate::query::HardFilter::Date`]
//! carries the absolute range task 26 already resolved, including relative
//! expressions (`before:last-week`) lexical.rs's own `day_start` cannot parse
//! at all. Reusing lexical.rs's compiler here would mean either handing it a
//! `QueryPlan` dependency it does not otherwise need, or discarding the
//! resolved range and re-deriving it from the raw string the way lexical.rs
//! does — silently losing relative-date support for five of the seven
//! retrievers for no reason. Duplicating the (short, mechanical, well-tested-
//! by-precedent) operator classification once here is the smaller risk than
//! either.
//!
//! # Hard filters, not scope
//!
//! See `retrieve`'s module docs for the integration decision this module is
//! the concrete answer to: every constraint this compiles comes from
//! [`crate::query::QueryPlan::hard_filters`]; [`crate::query::QueryPlan::scope`]
//! is never read.

use rusqlite::types::Value;

use crate::query::{AiPredicate, DateRange, Filter, HardFilter, HasTarget, IsFlag, Operator};

/// A hard-filter mask compiled to SQL: a boolean expression over `messages`
/// (and, via `EXISTS`/`IN` subqueries, `flags`/`attachments`/`mailboxes`/
/// `accounts`), plus the parameters it binds.
///
/// `sql` is safe to embed verbatim in a query — built only from fixed,
/// trusted column/table names this module wrote, never from filter *values*,
/// which always travel through `params` as bound parameters.
pub(crate) struct Mask {
    pub(crate) sql: String,
    pub(crate) params: Vec<Value>,
}

/// The result of compiling a plan's hard filters.
pub(crate) enum FilterMask {
    /// No filters, or none of them contribute a constraint.
    Unconstrained,
    /// At least one filter's positive form provably matches nothing, so —
    /// filters conjoin — the whole query matches nothing.
    ExcludesEverything,
    /// A real SQL predicate.
    Sql(Mask),
}

impl Mask {
    /// `sql`, wrapped as a correlated `EXISTS` gate against `messages` for a
    /// query whose driving table is something other than `messages` itself
    /// (`fts_messages.rowid`, a chunk's `message_id`, an entity mention's
    /// `message_id`, ...). Mirrors `retrieve::lexical`'s own `EXISTS` mask —
    /// see that module's docs for why a correlated `EXISTS` is used instead
    /// of `rowid IN (SELECT id FROM messages WHERE ...)`: the latter plans
    /// independently of the driving query and full-scans `messages`
    /// regardless of how few candidate rows actually need checking.
    pub(crate) fn exists_clause(&self, message_id_expr: &str) -> String {
        format!(
            "EXISTS (SELECT 1 FROM messages WHERE messages.id = {message_id_expr} AND {})",
            self.sql
        )
    }
}

/// What evaluating one hard filter's *positive* form (ignoring negation)
/// resolves to, before [`compile`] applies negation. Mirrors
/// `retrieve::lexical::RawEffect` exactly — see this module's docs for why
/// the two are not the same type.
enum RawEffect {
    /// A real SQL predicate over `messages`, invertible with `NOT (...)`.
    Sql(String, Vec<Value>),
    /// The positive form is false for every message today.
    Never,
    /// This stage cannot resolve the value with confidence in either
    /// direction. Unreachable for a date-shaped operator that resolved (that
    /// is [`HardFilter::Date`]); reachable for one that did not (task 26
    /// already tried and gave up — see [`HardFilter::Other`]'s docs).
    Unknown,
}

/// Compile `filters` into a [`FilterMask`].
pub(crate) fn compile(filters: &[HardFilter]) -> FilterMask {
    let mut clauses = Vec::new();
    let mut params = Vec::new();

    for hard in filters {
        let (effect, negated) = match hard {
            HardFilter::Date { filter, range } => (date_effect(range), filter.negated),
            HardFilter::Other(filter) => (classify(&filter.op), filter.negated),
        };
        match effect {
            RawEffect::Sql(sql, sql_params) => {
                clauses.push(if negated {
                    build_negated_clause(&sql)
                } else {
                    format!("({sql})")
                });
                params.extend(sql_params);
            }
            RawEffect::Never if !negated => return FilterMask::ExcludesEverything,
            RawEffect::Never => {
                tracing::debug!(
                    operator = operator_kind(hard.filter()),
                    "negation of an operator with no backing data yet is vacuously true; \
                     contributes no constraint"
                );
            }
            RawEffect::Unknown => {
                tracing::debug!(
                    operator = operator_kind(hard.filter()),
                    "operator value cannot be resolved at this stage; not applied as a gate"
                );
            }
        }
    }

    if clauses.is_empty() {
        FilterMask::Unconstrained
    } else {
        FilterMask::Sql(Mask {
            sql: clauses.join(" AND "),
            params,
        })
    }
}

/// A resolved date range's effect — always [`RawEffect::Sql`], since
/// [`HardFilter::Date`] only exists once task 26 has already produced an
/// absolute range.
fn date_effect(range: &DateRange) -> RawEffect {
    match (range.start, range.end) {
        (Some(start), Some(end)) => RawEffect::Sql(
            "COALESCE(date, internaldate) >= ? AND COALESCE(date, internaldate) < ?".to_owned(),
            vec![Value::Integer(start), Value::Integer(end)],
        ),
        (Some(start), None) => RawEffect::Sql(
            "COALESCE(date, internaldate) >= ?".to_owned(),
            vec![Value::Integer(start)],
        ),
        (None, Some(end)) => RawEffect::Sql(
            "COALESCE(date, internaldate) < ?".to_owned(),
            vec![Value::Integer(end)],
        ),
        // `before:`/`after:`/`on:`/`date:` always resolve at least one bound
        // when task 26 produces `HardFilter::Date` at all (see
        // `query::plan::resolve_filters`) — both `None` is unreachable in
        // practice, but degrading to "no constraint" rather than matching
        // nothing is the same fail-open rule every other unresolvable case
        // here follows.
        (None, None) => RawEffect::Unknown,
    }
}

/// Negate `sql` NULL-safely — see `retrieve::lexical::build_negated_clause`,
/// which this is byte-for-byte identical to: the same three-valued-logic
/// hazard applies to the same nullable columns regardless of which module's
/// mask a predicate ends up in.
fn build_negated_clause(sql: &str) -> String {
    format!("NOT COALESCE(({sql}), 0)")
}

fn operator_kind(filter: &Filter) -> &'static str {
    match &filter.op {
        Operator::From(_) => "from",
        Operator::To(_) => "to",
        Operator::Cc(_) => "cc",
        Operator::Subject(_) => "subject",
        Operator::Body(_) => "body",
        Operator::Has(_) => "has",
        Operator::Filename(_) => "filename",
        Operator::Larger(_) => "larger",
        Operator::Smaller(_) => "smaller",
        Operator::Before(_) => "before",
        Operator::After(_) => "after",
        Operator::On(_) => "on",
        Operator::DateRange(_, _) => "date",
        Operator::Is(_) => "is",
        Operator::Tag(_) => "tag",
        Operator::Note(_) => "note",
        Operator::In(_) => "in",
        Operator::Account(_) => "account",
        Operator::Thread(_) => "thread",
        Operator::Ai(_) => "ai",
    }
}

/// Classify one operator's positive form (everything except a resolved
/// date, which never reaches this function — see [`date_effect`]).
fn classify(op: &Operator) -> RawEffect {
    match op {
        Operator::From(value) => like_either("from_addr", "from_name", value),
        Operator::To(value) => like_one("to_addrs", value),
        Operator::Cc(value) => like_one("cc_addrs", value),
        Operator::Subject(value) => like_one("subject", value),
        Operator::Body(value) => like_one("body_text", value),
        Operator::Has(HasTarget::Attachment) => {
            RawEffect::Sql("has_attachments = 1".to_owned(), Vec::new())
        }
        // `has:note` is backed by the `notes` table (task 56); `has:tag` by
        // `tags`/`message_tags` (task 55). Both go through the shared helpers
        // below so this compiler and `retrieve::lexical`'s can never disagree.
        Operator::Has(HasTarget::Note) => {
            let (sql, params) = note_exists_sql();
            RawEffect::Sql(sql, params)
        }
        Operator::Has(HasTarget::Tag) => {
            let (sql, params) = has_tag_predicate_sql();
            RawEffect::Sql(sql, params)
        }
        Operator::Has(HasTarget::Other(_)) => RawEffect::Never,
        Operator::Filename(pattern) => RawEffect::Sql(
            "EXISTS (SELECT 1 FROM attachments WHERE attachments.message_id = messages.id \
             AND lower(attachments.filename) GLOB ?)"
                .to_owned(),
            vec![Value::Text(pattern.to_ascii_lowercase())],
        ),
        Operator::Larger(bytes) => size_cmp(">", *bytes),
        Operator::Smaller(bytes) => size_cmp("<", *bytes),
        // A date-shaped operator lands here only when task 26 could not
        // resolve its value (see `HardFilter::Other`'s docs) — the resolved
        // case is `HardFilter::Date`, handled by `date_effect` before
        // `classify` is ever called.
        Operator::Before(_) | Operator::After(_) | Operator::On(_) | Operator::DateRange(_, _) => {
            RawEffect::Unknown
        }
        Operator::Is(IsFlag::Unread) => flag_predicate("\\Seen", false),
        Operator::Is(IsFlag::Read) => flag_predicate("\\Seen", true),
        Operator::Is(IsFlag::Flagged) => flag_predicate("\\Flagged", true),
        Operator::Is(IsFlag::Replied) => flag_predicate("\\Answered", true),
        Operator::Is(IsFlag::Pinned | IsFlag::Muted) => RawEffect::Never,
        Operator::Is(IsFlag::Other(value)) => is_other_flag(value),
        // `tag:` is backed by `tags`/`message_tags` (task 55) and `note:` by
        // the `notes` table (task 56); `ai:` by `ai_summaries` (task 48) via
        // its own classifier below. Each shares its SQL with
        // `retrieve::lexical`'s classifier rather than re-deriving it.
        Operator::Tag(name) => {
            let (sql, params) = tag_predicate_sql(name);
            RawEffect::Sql(sql, params)
        }
        Operator::Note(value) => {
            let (sql, params) = note_text_sql(value);
            RawEffect::Sql(sql, params)
        }
        Operator::Ai(predicate) => match ai_predicate_sql(predicate) {
            Some((sql, params)) => RawEffect::Sql(sql, params),
            None => RawEffect::Never,
        },
        Operator::In(name) => RawEffect::Sql(
            "mailbox_id IN (SELECT id FROM mailboxes WHERE name = ? COLLATE NOCASE)".to_owned(),
            vec![Value::Text(name.clone())],
        ),
        Operator::Account(name) => RawEffect::Sql(
            "account_id IN (SELECT id FROM accounts WHERE name = ? COLLATE NOCASE)".to_owned(),
            vec![Value::Text(name.clone())],
        ),
        Operator::Thread(id) => match id.trim().parse::<i64>() {
            Ok(id) => RawEffect::Sql("thread_id = ?".to_owned(), vec![Value::Integer(id)]),
            Err(_) => RawEffect::Never,
        },
    }
}

/// Resolve an `ai:` predicate ([`crate::query::AiPredicate`], task 25's
/// parser) into a SQL fragment against `ai_summaries` (task 48, migration
/// V21) plus its bound parameters, or `None` if the key/value combination
/// is not recognized.
///
/// `pub(crate)`, and returning a plain `(String, Vec<Value>)` rather than
/// this module's own [`RawEffect`], on purpose: [`super::lexical`] carries
/// an independent, differently-typed `RawEffect` and, per that module's own
/// docs, does not reuse this module's *filter compiler* (`compile` vs
/// `compile_filters` — different input shapes, `HardFilter` vs `Filter`).
/// But classifying one already-parsed `AiPredicate` into SQL needs no
/// `QueryPlan` and no `HardFilter`/`Filter` distinction — there is nothing
/// about it that should differ between the two call sites — so this
/// function is the one place both `classify` below and
/// `lexical::classify` turn a predicate into SQL, and a future field added
/// to `ai_summaries` only has to be taught to match here once.
///
/// A message can carry more than one `ai_summaries` row (one per `pass` —
/// triage today, a deep row from task 49 later), so every shape below is an
/// `EXISTS` gate: "does *some* row for this message satisfy the predicate",
/// not a join that would multiply a message into one candidate row per
/// `ai_summaries` row it has.
///
/// A key or value this classifier does not recognize returns `None`, which
/// both callers turn into their own `RawEffect::Never` — the same choice
/// each already makes for a non-numeric `thread:` id or an `is:`-flag it
/// cannot resolve: a filter that cannot be evaluated with confidence must
/// exclude everything rather than silently match everything.
pub(crate) fn ai_predicate_sql(predicate: &AiPredicate) -> Option<(String, Vec<Value>)> {
    match predicate {
        AiPredicate::Flag(name) => ai_flag_sql(name),
        AiPredicate::Equals(key, value) => ai_equals_sql(key, value),
        AiPredicate::GreaterThan(key, value) => ai_threshold_sql(key, value),
    }
}

/// `ai:needs-reply` and friends — bare boolean flags. `needs_reply` is the
/// only one the PRD's grammar names; anything else is unrecognized.
fn ai_flag_sql(name: &str) -> Option<(String, Vec<Value>)> {
    match name.to_ascii_lowercase().as_str() {
        "needs-reply" | "needs_reply" => ai_exists("needs_reply = 1", Vec::new()),
        _ => None,
    }
}

/// `ai:category:invoice`, `ai:sentiment:negative`, `ai:priority:high` — exact
/// (case-insensitive) equality against one of `ai_summaries`' enum columns.
fn ai_equals_sql(key: &str, value: &str) -> Option<(String, Vec<Value>)> {
    let column = match key.to_ascii_lowercase().as_str() {
        "category" => "category",
        "sentiment" => "sentiment",
        "priority" => "priority",
        _ => return None,
    };
    ai_exists(
        &format!("{column} = ? COLLATE NOCASE"),
        vec![Value::Text(value.to_owned())],
    )
}

/// `ai:priority>high` — an ordinal comparison. `priority` is the only
/// `ai_summaries` column with a meaningful order (`category`/`sentiment` are
/// categorical, not ranked), so it is the only key this accepts; an
/// unrecognized key or an unrecognized priority level both return `None`
/// rather than guess at an ordering that was never defined.
fn ai_threshold_sql(key: &str, value: &str) -> Option<(String, Vec<Value>)> {
    if !key.eq_ignore_ascii_case("priority") {
        return None;
    }
    let rank = priority_rank(value)?;
    ai_exists(
        &format!("({PRIORITY_RANK_SQL}) > ?"),
        vec![Value::Integer(rank)],
    )
}

/// `low < normal < high < critical` as SQL, so a threshold comparison can be
/// pushed down without a scalar function. `ELSE -1`: a priority value this
/// build does not recognize (should not happen — `ai::triage::TriageResult::parse`
/// validates the enum before the row is ever written) sorts below every real
/// level rather than matching a threshold comparison it should not.
const PRIORITY_RANK_SQL: &str = "CASE priority \
     WHEN 'low' THEN 0 WHEN 'normal' THEN 1 WHEN 'high' THEN 2 WHEN 'critical' THEN 3 ELSE -1 END";

/// [`PRIORITY_RANK_SQL`]'s mapping, evaluated in Rust for the *comparison
/// value* a user typed (`ai:priority>high`'s `high`) — the column's own rank
/// is computed in SQL by the same table above so the two never drift apart.
fn priority_rank(value: &str) -> Option<i64> {
    match value.to_ascii_lowercase().as_str() {
        "low" => Some(0),
        "normal" => Some(1),
        "high" => Some(2),
        "critical" => Some(3),
        _ => None,
    }
}

/// `sql` (referencing `ai_summaries`' own unqualified columns), wrapped as a
/// correlated `EXISTS` against `ai_summaries.message_id = messages.id`.
fn ai_exists(sql: &str, params: Vec<Value>) -> Option<(String, Vec<Value>)> {
    Some((
        format!("EXISTS (SELECT 1 FROM ai_summaries WHERE ai_summaries.message_id = messages.id AND {sql})"),
        params,
    ))
}

/// Correlated condition: does a `notes` row belong to `messages`' own row —
/// directly (`notes.message_id = messages.id`) or via a thread-targeted note
/// on the same thread. This is the *same* "effective note" rule
/// `crate::notes::refresh_note_index` uses to decide which notes feed a
/// message's `index_content`/`fts_messages.notes` — a message-targeted note
/// counts for that message alone, a thread-targeted note counts for every
/// message currently in the thread. `note:`/`has:note` sharing this
/// definition with the free-text feed is what makes "this message's search
/// result mentions a note" and "`note:`/`has:note` matched this message"
/// describe the same thing.
const NOTE_TARGET_SQL: &str = "(notes.message_id = messages.id \
     OR (messages.thread_id IS NOT NULL AND notes.thread_id = messages.thread_id))";

/// `has:note`'s SQL: does *some* note (message- or effective-thread-scoped,
/// see [`NOTE_TARGET_SQL`]) exist for this message.
///
/// `pub(crate)`, and returning a plain `(String, Vec<Value>)` rather than
/// this module's own [`RawEffect`], for the identical reason
/// [`ai_predicate_sql`] does: [`super::lexical`] classifies `Operator`/
/// `HasTarget` values from a different input type
/// ([`crate::query::Filter`] vs [`HardFilter`]) but needs to turn
/// `HasTarget::Note` into *exactly* the same SQL this module does — sharing
/// this function is what makes that guaranteed rather than merely intended.
/// The task 56 acceptance bar this exists to meet: `note:`/`has:note` must
/// go through both retrievers' filter compilers identically, or a query
/// mixing free text with one of these operators silently drops the BM25 arm
/// for one of the two (exactly the bug task 48 shipped and this module's own
/// sharing of `ai_predicate_sql` already prevents for `ai:`).
pub(crate) fn note_exists_sql() -> (String, Vec<Value>) {
    (
        format!("EXISTS (SELECT 1 FROM notes WHERE {NOTE_TARGET_SQL})"),
        Vec::new(),
    )
}

/// `note:<value>`'s SQL: some note for this message (see [`NOTE_TARGET_SQL`])
/// contains `value`. A `LIKE` substring match against `notes.body_md` itself
/// — the same "hard filter reads the real column, not the ranked FTS index"
/// choice [`classify`]'s `subject:`/`body:` arms make (via [`like_one`]):
/// `note:` gates candidates, it does not rank them, so it has no reason to
/// route through `notes_fts`'s BM25 machinery the way a free-text term
/// would.
///
/// `pub(crate)` for the same sharing reason as [`note_exists_sql`].
pub(crate) fn note_text_sql(value: &str) -> (String, Vec<Value>) {
    (
        format!(
            "EXISTS (SELECT 1 FROM notes WHERE {NOTE_TARGET_SQL} AND notes.body_md LIKE ? ESCAPE '\\')"
        ),
        vec![Value::Text(like_pattern(value))],
    )
}

/// `tag:<name>` / `tag:<name>/*` -- matches a message's *effective* tags
/// (its own applied tags, or its thread's) within its own account.
/// `pub(crate)` (not `fn` private to this module) for the same reason
/// [`ai_predicate_sql`] is, and shared with the identical operator in two
/// more places: [`super::lexical::classify`] (see that module's own docs on
/// why it does not reuse this module's *filter compiler*, only individual
/// predicate classifiers like this one), and [`crate::tags::query`]'s
/// `BulkTag`-selector compiler, which needs the exact same "what does
/// `tag:x` mean" answer search does — task 48's own postmortem (a `tag:`-
/// shaped fix landing in only one of two retrievers, silently dropping the
/// BM25 arm from a mixed query) is exactly the failure a second,
/// independently-drifting copy would repeat.
///
/// # Queries `message_tags` directly, not the `messages_tags_effective` view
///
/// The view (migration V24) is `SELECT DISTINCT ...`, and `DISTINCT` blocks
/// SQLite's subquery flattener: a correlated `EXISTS` against the view
/// cannot push the outer row's `messages.id` down into an indexed lookup on
/// `message_tags`, so it degenerates into a full scan of `message_tags` (plus
/// a `TEMP B-TREE` for the `DISTINCT` itself) on *every* candidate row this
/// runs against — for `tag:`, that is every retriever that calls
/// [`compile`], on every query. Measured against a 200k-row `message_tags`
/// table, that is the difference between ~0.1ms and ~70ms per call, blowing
/// prd.md's <50ms Stage 1 budget on its own. This mirrors the exact
/// `rowid IN (SELECT ...)` hazard this module's own docs warn about for the
/// `EXISTS`-vs-`IN` choice generally -- a view is not exempt from it just for
/// being spelled as a `JOIN` instead of a subquery. The view still exists,
/// and is still the right tool, for `TagStore::list_tags`'s aggregate count
/// (one materialization per call, not per candidate row).
///
/// A trailing `/*` requests the tag *and its descendants*: `tag:project/*`
/// matches a tag named exactly `"project"` as well as any
/// `"project/alpha"`, `"project/alpha/q3"`, ... (tag names store the full
/// hierarchical path -- see `crate::tags::hierarchy`'s module docs).
/// Anything else matches that exact tag name only. Matching is
/// case-insensitive (`tags.name`'s own `COLLATE NOCASE`, migration V24 --
/// the same collation `UNIQUE(account_id, name)` uses, so `tag:Work` and
/// `tag:work` name the same tag a second `create_tag("Work", ...)` would
/// have collided with anyway).
///
/// Every `?` here is anonymous, never `?1`/`?2` — matching every other
/// classifier in this module (and in `retrieve::lexical`/`tags::query`).
/// SQLite numbers an explicit `?N` and an anonymous `?` from the *same*
/// index space, so an explicitly-numbered placeholder inside one fragment
/// silently collides with whichever index an anonymous `?` elsewhere in the
/// composed query happens to land on — `compile`'s own `clauses.join(" AND
/// ")` concatenates this fragment's SQL next to others that all use
/// anonymous `?`, and `tags::query::compile` additionally prepends its own
/// anonymous `account_id = ?` ahead of every filter. An earlier version of
/// this function used `?1`/`?2` and it was a real, caught-by-test bug:
/// `?1` collided with the *first* anonymous `?` in the composed statement
/// (here, that filter itself if it were first; in `tags::query::compile`,
/// `account_id`'s own placeholder), and `rusqlite` rejected the mismatched
/// parameter count outright rather than silently binding the wrong value —
/// still not something to rely on twice.
#[must_use]
pub(crate) fn tag_predicate_sql(name: &str) -> (String, Vec<Value>) {
    let exists = |extra: &str| -> String {
        format!(
            "EXISTS (SELECT 1 FROM message_tags mt \
             JOIN tags t ON t.id = mt.tag_id \
             WHERE mt.state = 'applied' \
             AND (mt.message_id = messages.id OR mt.thread_id = messages.thread_id) \
             AND t.account_id = messages.account_id \
             AND ({extra}))"
        )
    };
    match name.strip_suffix("/*") {
        // `tag:/*` is degenerate (no tag can be named the empty string) --
        // treated as an exact, unmatchable name rather than building an
        // unbounded `LIKE '%/...'` from an empty prefix, which would match
        // every hierarchical tag in the account.
        Some(prefix) if !prefix.is_empty() => {
            let sql = exists("t.name = ? OR t.name LIKE ? ESCAPE '\\'");
            let child_pattern = format!("{}/%", like_pattern_component(prefix));
            (
                sql,
                vec![Value::Text(prefix.to_owned()), Value::Text(child_pattern)],
            )
        }
        _ => (exists("t.name = ?"), vec![Value::Text(name.to_owned())]),
    }
}

/// `has:tag` -- any *applied* effective tag at all, regardless of which.
/// Shared between this module's `classify` and [`super::lexical::classify`]
/// for the identical reason [`tag_predicate_sql`] is (see its own docs) --
/// this exact predicate was duplicated verbatim in both files until this
/// extraction, exactly the drift `tag_predicate_sql`'s sharing exists to
/// prevent. No `tags` join is needed (unlike `tag_predicate_sql`): a
/// `message_tags` row's `tag_id` always names a tag in the same account as
/// the message it is attached to (`TagStore` resolves a target's account
/// before creating or looking up a tag, never across accounts), so an
/// unqualified "does any applied row exist" is already account-correct.
#[must_use]
pub(crate) fn has_tag_predicate_sql() -> (String, Vec<Value>) {
    (
        "EXISTS (SELECT 1 FROM message_tags mt WHERE mt.state = 'applied' \
         AND (mt.message_id = messages.id OR mt.thread_id = messages.thread_id))"
            .to_owned(),
        Vec::new(),
    )
}

/// Escape a `LIKE` pattern's own wildcards (`%`, `_`) and its escape
/// character, without the `%...%` substring wrapping [`like_pattern`]
/// applies -- [`tag_predicate_sql`] needs a *prefix*-anchored pattern
/// (`"prefix/%"`), not a substring one.
fn like_pattern_component(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn size_cmp(cmp: &str, bytes: u64) -> RawEffect {
    let bound = i64::try_from(bytes).unwrap_or(i64::MAX);
    RawEffect::Sql(format!("size {cmp} ?"), vec![Value::Integer(bound)])
}

fn flag_predicate(flag: &str, present: bool) -> RawEffect {
    let exists =
        "EXISTS (SELECT 1 FROM flags WHERE flags.message_id = messages.id AND flags.flag = ?)";
    let sql = if present {
        exists.to_owned()
    } else {
        format!("NOT {exists}")
    };
    RawEffect::Sql(sql, vec![Value::Text(flag.to_owned())])
}

fn is_other_flag(value: &str) -> RawEffect {
    let flag = match value.to_ascii_lowercase().as_str() {
        "draft" => "\\Draft".to_owned(),
        "deleted" => "\\Deleted".to_owned(),
        "recent" => "\\Recent".to_owned(),
        "answered" => "\\Answered".to_owned(),
        _ => value.to_owned(),
    };
    flag_predicate(&flag, true)
}

fn like_either(col_a: &str, col_b: &str, value: &str) -> RawEffect {
    let pattern = like_pattern(value);
    RawEffect::Sql(
        format!("({col_a} LIKE ? ESCAPE '\\' OR {col_b} LIKE ? ESCAPE '\\')"),
        vec![Value::Text(pattern.clone()), Value::Text(pattern)],
    )
}

fn like_one(col: &str, value: &str) -> RawEffect {
    RawEffect::Sql(
        format!("{col} LIKE ? ESCAPE '\\'"),
        vec![Value::Text(like_pattern(value))],
    )
}

fn like_pattern(value: &str) -> String {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("%{escaped}%")
}

#[cfg(test)]
mod tests;
