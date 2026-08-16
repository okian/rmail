-- V47: natural-language query compilation (task 58).
--
-- prd.md Stage 0 step 7 ("NL -> plan (Claude, cached)") and prd.md feature 13
-- ("Natural-Language Smart Folders"). Both are the same operation -- turn one
-- sentence of English into one query in rmail's own grammar -- and differ only
-- in what the caller does with the answer, so both go through the one cache
-- this migration adds.
--
-- # Why the compiled form is a query *string*, not a plan blob
--
-- Everything a plan needs (hard filters, lexical terms, quoted phrases,
-- negation) is already expressible in the operator grammar `query::parse`
-- owns, and that parser is the only thing in this build allowed to decide what
-- a query means. Storing the model's answer as a string it must re-parse keeps
-- exactly one definition of the grammar: a model that emits `from:stripe
-- invoice` gets the identical treatment as a user who typed it, including
-- after a future grammar addition teaches the parser something the stored
-- string already contained. A serialized plan blob would be a second, frozen
-- answer to "what does this query mean", and it is the one the model wrote.
--
-- It is also the boundary that keeps model output out of SQL. A `from:` value
-- becomes a bound parameter via `tags::query`, a free-text term becomes an
-- FTS5 quoted literal via `retrieve::lexical::quote_fts_literal`; neither ever
-- reaches a statement as a fragment.

CREATE TABLE query_plan_cache (
    -- Scoped per account even though nothing in a compiled plan is
    -- account-specific today. The cost is one row per (account, query); the
    -- alternative silently shares one account's compiled plan with another,
    -- which is a leak the moment compilation grows any corpus awareness (the
    -- contact graph is the obvious next input).
    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    -- SHA-256 of the *normalized* input (trimmed, whitespace-collapsed,
    -- lowercased) -- prd.md's "keyed by normalized query hash", so
    -- "Who owes me money?" and "who owes me money" share one compile.
    query_hash TEXT NOT NULL,
    -- The input as the user actually wrote it, for display. Never the key:
    -- two spellings that normalize together must not each pay for a call.
    raw TEXT NOT NULL,
    -- The compiled query, in rmail's operator grammar. Re-parsed on every
    -- read; see the header.
    compiled TEXT NOT NULL,
    -- The classified intent, as `query::plan::Intent`'s wire string.
    intent TEXT NOT NULL,
    -- The model's own one-line note about what it understood. Display only:
    -- nothing branches on it.
    notes TEXT NOT NULL,
    -- Which model produced this, so a plan compiled by a downgraded model is
    -- attributable after the fact.
    model TEXT NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    -- Cache bookkeeping. `uses` counts the reads served without a provider
    -- call, which is the only number that says whether this table is earning
    -- its keep.
    last_used_at INTEGER NOT NULL DEFAULT (unixepoch()),
    uses INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (account_id, query_hash)
) STRICT, WITHOUT ROWID;

-- A smart folder may now be defined in English and compiled once into a
-- hybrid plan: hard filters + FTS + an embedding predicate (prd.md feature
-- 13). Every column is nullable and NULL on every pre-existing row, which is
-- exactly what a deterministic folder (task 35) is -- so this migration
-- changes the meaning of no folder that already exists.
--
-- `predicate` keeps its meaning either way: the query string membership is
-- computed from. What changes for an NL folder is that free text in it is no
-- longer rejected, because there is now somewhere for it to go.
ALTER TABLE smart_folders ADD COLUMN nl_source TEXT;

-- The embedded free-text half of `predicate`, frozen at compile time.
--
-- Frozen, rather than embedded on each evaluation, is what makes "re-run
-- cheaply each sync" true: an evaluation of a hybrid folder makes no provider
-- call and no local embedder call at all, it runs one kNN against a vector
-- that is already on disk. It also makes membership stable -- an embedder
-- upgrade cannot silently redefine what an existing folder contains.
ALTER TABLE smart_folders ADD COLUMN query_vector BLOB;

-- Which embedding model produced `query_vector`. The dense arm joins on it,
-- so a re-index under a different model degrades that arm to nothing rather
-- than comparing vectors from two different spaces -- the same
-- `model`/`dim`/`content_hash` discipline `index::semantic` already applies.
ALTER TABLE smart_folders ADD COLUMN vector_model TEXT;

-- The cosine floor the dense arm admits a message at. Stored per folder
-- rather than read from a constant at evaluation time: a later change to the
-- default must not silently redefine what every existing folder contains.
ALTER TABLE smart_folders ADD COLUMN min_similarity REAL;

-- Which Claude model compiled `nl_source`, and when. Provenance only.
ALTER TABLE smart_folders ADD COLUMN compiled_model TEXT;
ALTER TABLE smart_folders ADD COLUMN compiled_at INTEGER;
