-- V11: chunks and their vectors.
--
-- Two tables because a chunk and its embedding have different lifetimes. A
-- chunk is a fact about the text: it changes only when the message's extracted
-- content changes. An embedding is a fact about the text *and the model*, so a
-- model change invalidates every vector while leaving every chunk intact.
-- Keeping them in one table would mean re-chunking to re-embed, and re-chunking
-- is what invalidates the spans a citation points at.
--
-- `vec_chunks` is a `sqlite-vec` virtual table. Virtual tables take no foreign
-- key and fire no cascade, so the link back to `chunks` is maintained by the
-- code that writes both, in one transaction, and checked by `index verify`.
CREATE TABLE chunks (
    chunk_id   INTEGER PRIMARY KEY,
    message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    -- Which `index_content` part this came from, so a hit can be attributed to
    -- the body, an attachment or a note.
    part       TEXT NOT NULL,
    -- Position within the part, from zero. Part plus ordinal is the identity a
    -- re-chunk has to reproduce for a stored vector to still mean anything.
    ordinal    INTEGER NOT NULL,
    -- Byte offsets into that part's normalized text. A citation quotes from
    -- the source rather than from a copy, so the text is not duplicated here.
    span_start INTEGER NOT NULL,
    span_end   INTEGER NOT NULL,
    -- Rough token count, for budgeting a context window without re-tokenizing.
    tokens     INTEGER NOT NULL,
    -- Hash of the chunk's text. What makes re-embedding conditional: content
    -- that did not change does not get embedded again, however often the
    -- indexer revisits the message.
    content_hash BLOB NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    UNIQUE (message_id, part, ordinal),
    CHECK (ordinal >= 0),
    CHECK (span_end > span_start),
    CHECK (tokens > 0)
) STRICT;

-- "What are this message's chunks" — the read a re-index, a deletion and a
-- citation renderer all do.
CREATE INDEX idx_chunks_message ON chunks(message_id);

-- Which model produced the vector for a chunk, and whether it is current.
--
-- Separate from `chunks` so a model switch is a delete here and nothing there,
-- and separate from `vec_chunks` because a virtual table cannot carry columns
-- that are not part of the index. `index verify` reconciles the three.
CREATE TABLE chunk_embeddings (
    chunk_id INTEGER PRIMARY KEY REFERENCES chunks(chunk_id) ON DELETE CASCADE,
    -- The model id, e.g. `bge-small-en-v1.5`. Vectors from different models are
    -- not comparable, so this is what makes drift detectable rather than
    -- silently wrong.
    model    TEXT NOT NULL,
    dim      INTEGER NOT NULL,
    -- The chunk's `content_hash` at the time it was embedded. A mismatch means
    -- the text moved under the vector.
    content_hash BLOB NOT NULL,
    embedded_at INTEGER NOT NULL DEFAULT (unixepoch()),
    CHECK (dim > 0),
    CHECK (model <> '')
) STRICT;

-- "Which chunks are stale for this model" — the read that drives a targeted
-- re-embed after a model switch.
CREATE INDEX idx_chunk_embeddings_model ON chunk_embeddings(model, dim);

-- The vectors themselves.
--
-- Fixed at 384 dimensions, which is `bge-small-en-v1.5` and the configured
-- default. `vec0` takes the dimensionality at creation time, so a different
-- model needs a different table and therefore a migration: making that explicit
-- is better than a table that silently accepts vectors it cannot compare, and
-- the `dim` column above is what turns a mismatch into an error rather than a
-- meaningless distance.
CREATE VIRTUAL TABLE vec_chunks USING vec0(
    chunk_id INTEGER PRIMARY KEY,
    embedding float[384]
);
