-- V12: one vector per message, alongside the per-chunk ones.
--
-- Chunk vectors answer "which passage is about this". A message vector answers
-- "which message is like this one" — the `mail similar 123` case — and it is a
-- different question: over chunks, a long thread wins simply by having more
-- chances to match, and deduplicating chunk hits back to messages after the
-- fact cannot recover the ranking that was lost inside the k limit.
--
-- The vector is the normalized mean of the message's chunk vectors. It costs no
-- extra model call, which matters because the alternative — embedding the whole
-- message as one string — both costs a call and truncates at the model's input
-- limit, so a long message's vector would describe only its opening.
CREATE VIRTUAL TABLE vec_messages USING vec0(
    message_id INTEGER PRIMARY KEY,
    embedding float[384]
);

-- Which model produced it, and from what.
--
-- Same split, and for the same reason, as `chunk_embeddings`: the virtual table
-- cannot carry columns that are not part of the index, and without the model id
-- a switch would leave vectors that are silently not comparable.
CREATE TABLE message_embeddings (
    message_id INTEGER PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,
    model      TEXT NOT NULL,
    dim        INTEGER NOT NULL,
    -- How many chunk vectors were averaged. Reported by `index verify`: a
    -- centroid over a different number of chunks than the message now has is
    -- stale even when every chunk vector is current.
    chunks     INTEGER NOT NULL,
    embedded_at INTEGER NOT NULL DEFAULT (unixepoch()),
    CHECK (dim > 0),
    CHECK (chunks > 0),
    CHECK (model <> '')
) STRICT;

CREATE INDEX idx_message_embeddings_model ON message_embeddings(model, dim);
