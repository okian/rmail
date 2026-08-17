-- V54: the learned L1 ranker's model store (task 65).
--
-- prd.md, "Personalization & Implicit-Feedback Learning Loop": a local job
-- "trains the L1 GBDT / updates linear weights on the accumulated pairs,
-- evaluates on a held-out slice, and **hot-swaps** the model only if offline
-- NDCG improves (guardrail against regressions). Old model kept for rollback."
--
-- One table. Every row is one training run's candidate model, *including the
-- ones the guardrail refused*, and the columns that decide the verdict are
-- stored beside the weights that were judged by them.
--
-- # Why a rejected candidate is a row and not a discarded value
--
-- The guardrail is the whole point of this task, and a guardrail nobody can
-- see refuse anything is indistinguishable from one that never fires. Keeping
-- the refused candidate — with the two NDCG numbers that refused it — is what
-- makes `mail search models` answer "did last night's run try to make search
-- worse, and by how much" rather than only "nothing changed". It is also the
-- only way an operator can tell a run that produced a worse model from a run
-- that found no data to train on at all; both leave the active model alone.
--
-- A rejected row can never be activated. See `status` below.
--
-- # Why `active` is nullable rather than a 0/1 flag
--
-- SQLite's UNIQUE indexes treat NULLs as distinct from each other, so
-- `active INTEGER` holding either 1 or NULL, with a plain UNIQUE index over
-- it, is a schema-level guarantee that **at most one model is live** — the
-- invariant a hot-swap has to hold across a crash between the demote and the
-- promote. A `NOT NULL DEFAULT 0` column could not carry that constraint
-- (every inactive row would collide), and enforcing "exactly one" in Rust
-- would put the invariant somewhere a half-applied transaction can break it.
--
-- `active = 1` is therefore "this is the live model" and NULL is "it is not";
-- the richer lifecycle lives in `status`.
--
-- # Why the weights are a blob and not a child table
--
-- One row per feature would be 34 rows per model and would invite exactly the
-- mistake `rank::l1`'s own docs argue against: a positional feature id kept in
-- sync with `FeatureName` by hand. The blob is the versioned JSON envelope
-- `rank::train::model::encode` writes — name-keyed, so a feature added later
-- is a `serde` default rather than a reinterpretation of every stored model,
-- and a name this build does not know is a load *failure* rather than a
-- silently mis-scored ranking. `search_impression.features` is stored the same
-- way and for the same reason.

CREATE TABLE ranker_model (
    id             INTEGER PRIMARY KEY,
    created_at     INTEGER NOT NULL DEFAULT (unixepoch()),
    -- Which model family the blob holds. 'linear' is the only one this build
    -- writes or reads: task 65 updates `rank::l1::Weights` rather than growing
    -- a tree ensemble (see `rank::train`'s module docs for why). The column
    -- exists so a future GBDT is a new value here plus a decoder, not a
    -- migration that has to reinterpret existing rows.
    kind           TEXT NOT NULL DEFAULT 'linear',
    weights        BLOB NOT NULL,
    -- What the run was trained on. Recorded rather than derived: retention
    -- deletes the feedback rows these counts came from, so a month-old model's
    -- provenance is only knowable if it was written down at the time.
    train_queries  INTEGER NOT NULL DEFAULT 0,
    train_pairs    INTEGER NOT NULL DEFAULT 0,
    -- The held-out slice the guardrail measured on, and how many of those
    -- queries carried any engagement at all. The second number is the one that
    -- says whether the verdict means anything: NDCG over a slice nobody
    -- clicked in is 0.0 for every model, which is a tie, not a comparison.
    eval_queries   INTEGER NOT NULL DEFAULT 0,
    eval_engaged   INTEGER NOT NULL DEFAULT 0,
    -- The two numbers the verdict is a comparison of: the model that was live
    -- when this candidate was judged, and the candidate, both scored by
    -- `eval::replay::shadow` over the *same* held-out impressions. Stored as
    -- the pair rather than as their difference so a later reader can see which
    -- side moved.
    baseline_ndcg  REAL NOT NULL DEFAULT 0,
    candidate_ndcg REAL NOT NULL DEFAULT 0,
    -- 'accepted' — beat the live model by at least `search.training.min_ndcg_gain`
    --              on the held-out slice. Eligible to be `active`, now or via
    --              a rollback.
    -- 'rejected' — did not. Kept for the audit trail and can never be made
    --              active: an operator who could hand-activate a candidate the
    --              guardrail refused would be able to undo the guardrail, which
    --              is the one thing this task exists to prevent. Re-training is
    --              the way forward from a rejected run.
    status         TEXT NOT NULL,
    active         INTEGER,
    -- Free text for the operator surface: which run wrote this, or why a
    -- rollback moved off it.
    note           TEXT NOT NULL DEFAULT '',
    CHECK (kind IN ('linear')),
    CHECK (status IN ('accepted', 'rejected')),
    CHECK (active IS NULL OR active = 1),
    -- A rejected candidate is never live. Belt and braces with the Rust side,
    -- because this is the constraint the whole task rests on and it costs one
    -- line to have the database refuse it too.
    CHECK (active IS NULL OR status = 'accepted'),
    CHECK (train_queries >= 0),
    CHECK (train_pairs >= 0),
    CHECK (eval_queries >= 0),
    CHECK (eval_engaged >= 0 AND eval_engaged <= eval_queries)
) STRICT;

-- At most one live model. See the header.
CREATE UNIQUE INDEX idx_ranker_model_active ON ranker_model(active);

-- `mail search models` lists newest first, and a rollback selects "the newest
-- accepted model older than the live one" — both are this index.
CREATE INDEX idx_ranker_model_history ON ranker_model(status, id DESC);
