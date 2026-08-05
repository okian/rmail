-- V8: normalized text, one row per indexable part of a message.
--
-- Everything searchable is derived from here rather than from `messages`
-- directly. Two reasons that separation earns its keep:
--
-- A message is not one document. A subject, a body, a note the user attached,
-- and an AI summary are different things with different weights in ranking and
-- different lifetimes -- a summary is rewritten when the model changes, a body
-- never is. Storing them as one blob would make every one of those a rewrite of
-- all of them.
--
-- And the text stored here is *normalized*, not original. Whitespace collapsed,
-- control characters dropped, HTML already stripped. The original is still in
-- `messages.raw`; this is the form the indexes agree on, and `content_hash` is
-- over exactly these bytes -- which is what makes "has this changed?" a
-- comparison rather than a re-extraction.
CREATE TABLE index_content (
    message_id   INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    -- subject | headers | body | attachment:<part-id> | note | summary
    part         TEXT NOT NULL,
    mime         TEXT,
    -- ISO 639-1 where the detector was confident enough to say. NULL is a real
    -- answer -- a two-word subject has no detectable language, and guessing
    -- would pick the wrong stemmer.
    lang         TEXT,
    text         TEXT NOT NULL,
    chars        INTEGER NOT NULL,
    -- Over the normalized text above. The whole re-index decision rests on it.
    content_hash BLOB NOT NULL,
    extracted_at INTEGER NOT NULL DEFAULT (unixepoch()),
    -- Which extractor produced it, so a fixed extractor's output can be told
    -- from a broken one's without re-reading the mail.
    extractor    TEXT,
    PRIMARY KEY (message_id, part)
) STRICT;

-- No index on `message_id` alone: `PRIMARY KEY (message_id, part)` already
-- creates one whose leading column serves it, and a second would be pure write
-- amplification on the hottest write path in the indexer.
