-- V34: the implicit-feedback learning loop's log (task 64).
--
-- prd.md, "Personalization & Implicit-Feedback Learning Loop": every search
-- interaction is logged locally and periodically distilled into ranker
-- weights. Three tables, one per grain: the query (`search_log`), what it
-- showed and where (`search_impression`), and what the user then did
-- (`search_action`).
--
-- # Local telemetry, and only that
--
-- prd.md is unambiguous: "Logging is strictly opt-outable (`search.learning =
-- false`); it is local telemetry, never transmitted." Nothing in this schema
-- carries an upload cursor, a "synced" flag, or a device id, because there is
-- no destination for any of them. `raw_query` is the user's literal search
-- text and `search_impression.features` is a full behavioural fingerprint of
-- their mailbox; both stay in this file. The opt-out is enforced one layer
-- up, in `rmail_core::feedback::FeedbackStore`, which writes *nothing at all*
-- when learning is off rather than writing rows and filtering them later.
--
-- # `message_id`, not prd.md's `message_uid`
--
-- prd.md's sketch names the column `message_uid`. That name is actively
-- dangerous against this schema: `messages.uid` is a real column and holds
-- the *IMAP UID*, which is a different number from `messages.id`, is only
-- unique within `(mailbox_id, uidvalidity)`, and is reassigned wholesale on a
-- UIDVALIDITY bump. Every other feature table here (`message_tags`, `notes`,
-- `index_content`, ...) keys on `messages.id`, the ranking pipeline speaks
-- `messages.id` end to end (`fuse::FusedCandidate::message_id`,
-- `features::CandidateFeatures::message_id`, `rank::RankedCandidate`), and a
-- column named `..._uid` sitting next to a table that has a genuine `uid` is
-- an invitation to write the wrong one into it. The column is the row id.

-- One row per ranked search that actually produced results.
--
-- `query_id` is assigned by `rmail_core::feedback` before the search streams,
-- not by SQLite's rowid allocator, and that is load-bearing rather than
-- incidental. A client has to be able to attribute an action ("the user
-- opened result 3") back to the query that produced it, which means the id
-- has to be on every `SearchHit` as it is streamed -- i.e. known *before*
-- anything is written. Taking a rowid instead would put a synchronous INSERT
-- on the single writer connection ahead of the first hit, inside prd.md's
-- 30 ms first-paint budget and behind whatever sync batch happens to hold the
-- writer. See `feedback::new_query_id` for how the id is generated and why a
-- collision is a dropped log line rather than a failed search.
CREATE TABLE search_log (
    query_id     INTEGER PRIMARY KEY,
    -- The account the search was scoped to, or NULL for "every configured
    -- account" (`SearchRequest.account_id = 0`). Cascades: deleting an
    -- account removes the record of what was searched inside it, which is
    -- the only defensible reading of deleting an account.
    account_id   INTEGER REFERENCES accounts(id) ON DELETE CASCADE,
    -- The query exactly as it reached the planner, operators and all. Stored
    -- verbatim for the same reason `saved_searches.query` is: task 65 has to
    -- be able to re-plan it, and `query::parse` is lossless over the raw
    -- string by design.
    raw_query    TEXT NOT NULL,
    -- SHA-256 over the normalized query text. Lets the trainer group repeats
    -- of "the same search" (whitespace/case/Unicode-composition differences
    -- are not different queries) without re-normalizing every row, and lets a
    -- future A/B bucket a query deterministically, as prd.md's evaluation
    -- section describes.
    norm_hash    BLOB NOT NULL,
    -- The classified intent, as its lowercase name. NOT cosmetic: the L1
    -- scorer *gates on it* (`rank::l1::bulk_downweight_suppressed` zeroes the
    -- newsletter/automated weights under a navigational intent), so replaying
    -- an impression's stored feature vector without its intent reproduces a
    -- different score than the one the user actually saw.
    intent       TEXT,
    issued_at    INTEGER NOT NULL,
    -- How many impressions this query logged. Denormalized on purpose: the
    -- pruner and any "how much data do I have" question read it without
    -- touching the (much larger) impression table.
    result_count INTEGER
) STRICT;

-- Retention selects by `issued_at` (rows past the age horizon) and resolves
-- its row-count bound with `ORDER BY issued_at DESC, query_id DESC
-- LIMIT -1 OFFSET ?`. Both are this index; without it each prune pass is a
-- full scan plus a sort of the whole log.
--
-- Note the sweep deletes in chunks and does *not* order within a chunk: a
-- completed sweep leaves the same rows either way, and the loop only exits
-- once the doomed set is empty, so per-chunk ordering would buy nothing but
-- a sort.
CREATE INDEX idx_search_log_issued ON search_log(issued_at);

-- What the query showed, and the exact feature vector it was ranked by.
--
-- prd.md: "The **feature vector** of every impression (for exact replay)."
-- `features` is a versioned JSON envelope produced by
-- `rmail_core::feedback::encode_features` -- see that function's docs for the
-- format, which task 65 decodes. It is deliberately the *serialized* vector
-- rather than a foreign key into some feature-cache table: the whole point is
-- that the numbers survive a re-index, a corpus change, a weight change, and
-- a deleted message, none of which a re-derivation would.
--
-- `message_id` carries no REFERENCES clause, unlike every other per-message
-- table in this schema, and prd.md's own sketch does not give it one either.
-- The reason is the sentence above: an impression is self-contained training
-- data. Cascading it away when the message is expunged would silently delete
-- the corpus every time a mailbox is cleaned up -- exactly the mail the user
-- searched for and acted on. Growth is bounded by retention (see
-- `feedback::FeedbackStore::prune`), not by message lifetime.
CREATE TABLE search_impression (
    query_id    INTEGER NOT NULL REFERENCES search_log(query_id) ON DELETE CASCADE,
    message_id  INTEGER NOT NULL,
    -- 1-based rank in the page the user was actually shown, top result first.
    -- 1-based rather than 0-based so prd.md's position-bias correction can
    -- write its examination propensity as `1/position^eta` (or
    -- `1/log2(1+position)`) with no special case at the top of the page --
    -- the one position that occurs most often.
    position    INTEGER NOT NULL,
    features    BLOB NOT NULL,
    -- The Stage 4 score this candidate actually got, from the ranker that was
    -- live at the time. Stored alongside the features so task 65 can verify a
    -- replay reproduces it before trusting a decoded vector.
    l1_score    REAL,
    -- Stage 5's rerank score. Always NULL until task 51 ships an L2 stage;
    -- the column exists now so an L2 rollout does not need a migration and,
    -- more importantly, so a trainer can tell "no reranker ran" from "the
    -- reranker scored zero".
    l2_score    REAL,
    PRIMARY KEY (query_id, message_id),
    CHECK (position >= 1)
) STRICT;

-- What the user did with a result. prd.md's vocabulary exactly:
-- open | reply | archive | dwell | scroll_past.
--
-- Open TEXT rather than a SQL-level enum, matching this schema's existing
-- convention for small vocabularies owned by application code
-- (`ai_summaries.pass`, `message_tags.source`, `notes.author`) --
-- `rmail_core::feedback::ActionKind` is the closed enum that enforces the
-- vocabulary on every write this build makes, and the gRPC boundary rejects
-- anything outside it with INVALID_ARGUMENT.
--
-- No unique constraint and no primary key: the same result can legitimately
-- be opened twice, or opened and then archived, and each occurrence is its
-- own observation. Repetition is signal here, not duplication.
CREATE TABLE search_action (
    query_id    INTEGER NOT NULL REFERENCES search_log(query_id) ON DELETE CASCADE,
    message_id  INTEGER NOT NULL,
    action      TEXT NOT NULL,
    -- Milliseconds spent on the message, for `dwell`. NULL for every other
    -- action: prd.md ranks "reply/long-dwell > open > hover", so how long
    -- must be distinguishable from "not measured", and 0 is a real dwell.
    dwell_ms    INTEGER,
    at          INTEGER NOT NULL,
    CHECK (dwell_ms IS NULL OR dwell_ms >= 0)
) STRICT;

-- SQLite resolves an ON DELETE CASCADE by scanning the referencing table for
-- the parent key, so without an index on `query_id` every pruned `search_log`
-- row costs a full scan of this table -- and pruning deletes in chunks of
-- thousands. `search_impression`'s own cascade is already served by its
-- `(query_id, message_id)` primary key.
--
-- The same index serves the trainer's only read shape ("every action for this
-- query"), so it is not carried for the pruner alone.
CREATE INDEX idx_search_action_query ON search_action(query_id);
