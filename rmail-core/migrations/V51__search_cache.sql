-- V51: query/embedding/result caching and incrementality (task 36).
--
-- prd.md "Caching & Incrementality" names three caches. One of them already
-- exists: `query_plan_cache` (V47, task 58) is prd.md's query-plan cache,
-- keyed on a normalized query hash per account, and this migration does not
-- touch it. Document embeddings are likewise already cached, by
-- `chunk_embeddings`.`content_hash` (V13) -- that is what makes "documents
-- re-embedded only on content_hash change" true today. What is missing, and
-- what this migration adds, is the other half of the embedding cache (query
-- vectors) and the result cache, plus the corpus version the result cache is
-- keyed on.
--
-- # The one rule every table here obeys
--
-- A cache that can return a stale answer is worse than no cache, and search
-- relevance is this product's first feature. So nothing below is invalidated
-- by *deleting* a row when the world changes -- a deletion someone has to
-- remember to write, and therefore a deletion someone will forget. Every
-- entry is instead **content-addressed on everything that could change its
-- answer**: change the corpus, the ranker weights, the embedding model or the
-- rerank backend, and the lookup computes a different key and misses. Stale
-- rows are then merely garbage (swept on a bound), never answers.

-- ---------------------------------------------------------------------------
-- Corpus version
-- ---------------------------------------------------------------------------

-- A monotonic counter that changes whenever anything a search could match
-- changes. It is the `corpus_version` in prd.md's result-cache key
-- `(query, filter, corpus_version)`.
--
-- # Why one global row rather than one per account
--
-- A per-account counter is more precise: new mail in account B would not
-- invalidate account A's cached results. It is also a correctness cliff. A
-- search is not always scoped to one account (`SearchRequest.account_id = 0`
-- means "every configured account"), threads and tags cross accounts, and the
-- moment any future retriever grows a cross-account input, a per-account
-- counter starts *under*-invalidating -- serving an answer computed before a
-- change that affected it. One global counter can only ever
-- over-invalidate: the failure mode is a cache miss and a recomputed search,
-- which costs milliseconds and is always correct. That is the right side of
-- the trade for a cache whose whole justification is that its answers are
-- trustworthy.
CREATE TABLE corpus_version (
    -- Exactly one row, forever. The CHECK is what makes that a schema
    -- guarantee rather than a convention the triggers below happen to keep.
    id INTEGER PRIMARY KEY CHECK (id = 0),
    -- Incremented by every trigger below. Never decreases, so a cached entry
    -- stamped with an older version can never be mistaken for a current one.
    version INTEGER NOT NULL,
    -- When it last moved, in whole seconds. Read for the "freshly-synced mail
    -- bypasses the result cache" rule: for a short window after the corpus
    -- moves, the result cache is not consulted at all, so mail that has only
    -- just landed cannot be hidden behind an answer computed a moment before
    -- it arrived -- even by a change no trigger below happens to observe.
    changed_at INTEGER NOT NULL
) STRICT;

INSERT INTO corpus_version (id, version, changed_at) VALUES (0, 0, unixepoch());

-- # Which tables bump it, and why exactly these five
--
-- `messages` is the corpus itself. `flags` decides `is:unread`/`is:flagged`,
-- and IMAP flag sync writes it without touching `messages`. `message_tags`
-- decides `tag:`, and tagging likewise leaves `messages` alone.
-- `index_content` is the normalized text extraction produces.
--
-- `index_state` is the one that is not obvious, and leaving it out was a real
-- staleness hole. The four tables above are the *inputs* to indexing; the
-- things retrieval actually reads are `fts_messages`, `chunks`/`vec_chunks`
-- and `entities`/`entity_mentions`, and each is written by a separate queue
-- stage that reads `index_content` and writes only its own table. So mail
-- could land (bump), extraction could run (bump), a search could be cached --
-- and then the semantic stage could drain and bring that message into the
-- dense arm with no bump at all, leaving the identical query serving its
-- pre-embedding answer for a whole TTL. `index_state` closes it: every stage
-- records its completion there, in the same transaction that marks the queue
-- job done (`index::queue`), so "a stage finished some work" is exactly one
-- write away from a version bump no matter which stage it was. It closes the
-- destructive direction too -- `IndexAdmin::wipe_stage` deletes
-- `index_state` rows for the stage it is rebuilding, so
-- `mail index rebuild --kind semantic` now invalidates cached results even
-- though it touches none of the four tables above.
--
-- Two of the interesting tables could not have been covered directly anyway:
-- `fts_messages` is an FTS5 virtual table and `vec_chunks` a `vec0` one, and
-- SQLite takes no trigger on either.
--
-- Triggers rather than a `bump()` a write path calls: this is enforcement the
-- schema does, not discipline every present and future writer has to
-- remember. A trigger cannot be forgotten by a new code path, cannot be
-- skipped by a bulk `INSERT ... SELECT`, and fires inside the same
-- transaction as the write, so a reader never sees a changed corpus under an
-- unchanged version.
CREATE TRIGGER corpus_version_messages_insert AFTER INSERT ON messages BEGIN
    UPDATE corpus_version SET version = version + 1, changed_at = unixepoch() WHERE id = 0;
END;
CREATE TRIGGER corpus_version_messages_update AFTER UPDATE ON messages BEGIN
    UPDATE corpus_version SET version = version + 1, changed_at = unixepoch() WHERE id = 0;
END;
CREATE TRIGGER corpus_version_messages_delete AFTER DELETE ON messages BEGIN
    UPDATE corpus_version SET version = version + 1, changed_at = unixepoch() WHERE id = 0;
END;

CREATE TRIGGER corpus_version_flags_insert AFTER INSERT ON flags BEGIN
    UPDATE corpus_version SET version = version + 1, changed_at = unixepoch() WHERE id = 0;
END;
CREATE TRIGGER corpus_version_flags_update AFTER UPDATE ON flags BEGIN
    UPDATE corpus_version SET version = version + 1, changed_at = unixepoch() WHERE id = 0;
END;
CREATE TRIGGER corpus_version_flags_delete AFTER DELETE ON flags BEGIN
    UPDATE corpus_version SET version = version + 1, changed_at = unixepoch() WHERE id = 0;
END;

CREATE TRIGGER corpus_version_message_tags_insert AFTER INSERT ON message_tags BEGIN
    UPDATE corpus_version SET version = version + 1, changed_at = unixepoch() WHERE id = 0;
END;
CREATE TRIGGER corpus_version_message_tags_update AFTER UPDATE ON message_tags BEGIN
    UPDATE corpus_version SET version = version + 1, changed_at = unixepoch() WHERE id = 0;
END;
CREATE TRIGGER corpus_version_message_tags_delete AFTER DELETE ON message_tags BEGIN
    UPDATE corpus_version SET version = version + 1, changed_at = unixepoch() WHERE id = 0;
END;

CREATE TRIGGER corpus_version_index_content_insert AFTER INSERT ON index_content BEGIN
    UPDATE corpus_version SET version = version + 1, changed_at = unixepoch() WHERE id = 0;
END;
CREATE TRIGGER corpus_version_index_content_update AFTER UPDATE ON index_content BEGIN
    UPDATE corpus_version SET version = version + 1, changed_at = unixepoch() WHERE id = 0;
END;
CREATE TRIGGER corpus_version_index_content_delete AFTER DELETE ON index_content BEGIN
    UPDATE corpus_version SET version = version + 1, changed_at = unixepoch() WHERE id = 0;
END;

-- "Some indexing stage finished, or was wiped." See the header above for why
-- this is the trigger that makes the other four honest.
CREATE TRIGGER corpus_version_index_state_insert AFTER INSERT ON index_state BEGIN
    UPDATE corpus_version SET version = version + 1, changed_at = unixepoch() WHERE id = 0;
END;
CREATE TRIGGER corpus_version_index_state_update AFTER UPDATE ON index_state BEGIN
    UPDATE corpus_version SET version = version + 1, changed_at = unixepoch() WHERE id = 0;
END;
CREATE TRIGGER corpus_version_index_state_delete AFTER DELETE ON index_state BEGIN
    UPDATE corpus_version SET version = version + 1, changed_at = unixepoch() WHERE id = 0;
END;

-- ---------------------------------------------------------------------------
-- Embedding cache (the query half)
-- ---------------------------------------------------------------------------

-- prd.md: "query and document embeddings persisted; documents re-embedded
-- only on content_hash change." The document half lives in
-- `chunk_embeddings`/`vec_chunks` and is unchanged by this migration. This
-- table is the query half: the vector for a piece of text that is not a
-- document -- a search box's contents, a smart folder's prose, an
-- attachment query -- which until now was recomputed on every keystroke-driven
-- re-search, and on a hosted backend was a paid network round trip each time.
--
-- # Invalidation: there is none, by construction
--
-- The key is `(model, dim, sha256(truncated text))`. A model swap, a width
-- change, or one different character all produce a different key, so no row
-- here can ever answer for input it was not computed from. That is the same
-- `model`/`dim`/`content_hash` discipline `index::semantic` already applies to
-- document vectors, and it means this table needs no invalidation pass at
-- all -- only a size bound, below.
CREATE TABLE embedding_cache (
    -- The embedder's model id, exactly as `Embedder::model` reports it.
    model TEXT NOT NULL,
    -- The model's width. Redundant with `length(vector) / 4` and stored anyway:
    -- it is what `Embedding::from_bytes` checks the blob against, so a
    -- truncated row is rejected as corruption instead of being read as a
    -- shorter vector that would score zero against everything and sort last.
    dim INTEGER NOT NULL,
    -- SHA-256 of the input *after* `embed::truncate` -- the same bytes the
    -- backend would actually see. Hashing the untruncated text instead would
    -- file two inputs sharing an 8 KiB prefix under two keys for one vector:
    -- wasteful, not wrong, but there is no reason to prefer it.
    text_hash BLOB NOT NULL,
    -- Little-endian f32s, `Embedding::to_bytes`.
    vector BLOB NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    -- LRU bookkeeping. `uses` is also the only evidence this table earns its
    -- keep -- a cache nobody can measure is a cache nobody can justify.
    last_used_at INTEGER NOT NULL DEFAULT (unixepoch()),
    uses INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (model, dim, text_hash)
) STRICT, WITHOUT ROWID;

-- Eviction reads this in ascending order; without it, every insert past the
-- bound would scan the table to find the coldest rows.
CREATE INDEX embedding_cache_lru ON embedding_cache (last_used_at);

-- ---------------------------------------------------------------------------
-- Result cache
-- ---------------------------------------------------------------------------

-- prd.md: "`(query, filter, corpus_version)` -> ranked ids, invalidated when
-- the corpus version bumps (new mail) or the active ranker changes."
--
-- # What is in the key
--
-- `cache_key` is a SHA-256 over the request (query text, filter, account
-- scope, mode, limit, rerank policy, search kind), the corpus version, and a
-- *ranker fingerprint* -- a digest of the entire effective `[search]` config
-- plus the embedding model and width. Both of prd.md's invalidation triggers
-- are therefore structural: new mail changes `corpus_version` and the key
-- moves; retuning a rank weight, switching the fusion strategy, changing the
-- rerank backend or the embedding model changes the fingerprint and the key
-- moves. Neither requires a `DELETE` anyone could forget to write.
--
-- # Why the two hashed inputs are also stored as columns
--
-- They are already inside `cache_key`, so re-checking them on read is
-- strictly redundant -- and it is checked anyway, because "redundant given no
-- bug" is exactly the assumption a stale search result would be hiding
-- behind. A key-construction mistake that made two different corpus versions
-- collide would otherwise surface as quietly wrong search results; with the
-- columns it surfaces as a miss. They also make a targeted sweep ("drop
-- everything from before the current version") expressible in SQL.
CREATE TABLE search_result_cache (
    -- SHA-256 of the whole key tuple; see above.
    cache_key BLOB PRIMARY KEY,
    -- The `corpus_version.version` this answer was computed against.
    corpus_version INTEGER NOT NULL,
    -- SHA-256 of the `[search]` config plus embedding model/width.
    ranker_fingerprint BLOB NOT NULL,
    -- The ranked message ids, best first, as little-endian i64s. A blob
    -- rather than a child table: this value is read and written whole, always,
    -- and its order is its meaning -- a row-per-id table would need an
    -- explicit ordinal column to say the same thing and a join to read it.
    message_ids BLOB NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    last_used_at INTEGER NOT NULL DEFAULT (unixepoch()),
    uses INTEGER NOT NULL DEFAULT 0
) STRICT, WITHOUT ROWID;

CREATE INDEX search_result_cache_lru ON search_result_cache (last_used_at);
-- Lets the sweep drop every entry from a superseded corpus version without a
-- full scan.
CREATE INDEX search_result_cache_version ON search_result_cache (corpus_version);
