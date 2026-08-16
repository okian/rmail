-- V39: a lexical index at *attachment* granularity.
--
-- `fts_messages` (V9) already indexes attachment text -- in an `attachments`
-- column, on a row whose `rowid` is `messages.id`. That is the right shape for
-- ranking *messages*: a term in a PDF is evidence about the mail that carried
-- it. It is the wrong shape for the question this table exists to answer,
-- which is "*which* attachment, and which page". A message with a signed
-- contract and a signature-block logo has one `fts_messages` row covering
-- both, and no way to say the clause came from the first.
--
-- So: one row per extracted attachment, `rowid` mapping to
-- `(message_id, part_id)` through `attachment_docs`. A hit therefore names a
-- part, and a byte offset inside that part resolves to a page through
-- `attachment_pages` (V13) with no second copy of the text anywhere.
--
-- Contentless (`content=''`) with `contentless_delete=1`, for the reasons V9
-- gives at length: the text already lives in `index_content`, and a second
-- copy would be the largest table in the database again. Same tokenizer as V9
-- (`remove_diacritics 2`), so a query that finds "café" in a body finds it in
-- a PDF too -- two lexical indexes that disagreed about what a character is
-- would be worse than one.
CREATE TABLE attachment_docs (
    -- The FTS5 `rowid`. An `INTEGER PRIMARY KEY` (rowid alias) rather than a
    -- composite key because a contentless FTS5 table is addressed by rowid and
    -- nothing else; this is the mapping table that makes that addressable.
    doc_id     INTEGER PRIMARY KEY,
    message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    -- Matches `attachment_extractions.part_id` and the `attachment:<part_id>`
    -- key `index_content` stores the text under.
    part_id    TEXT NOT NULL,
    UNIQUE (message_id, part_id)
) STRICT;

-- "Every attachment of this message", which is how the extraction pipeline
-- reconciles what it just wrote against what was there before.
CREATE INDEX idx_attachment_docs_message ON attachment_docs(message_id);

CREATE VIRTUAL TABLE fts_attachments USING fts5(
    text,
    content = '',
    contentless_delete = 1,
    tokenize = "unicode61 remove_diacritics 2"
);

-- A deleted message must leave this index with it. `messages` cascades to
-- `attachment_docs`, but a virtual table takes no foreign key and SQLite only
-- fires DELETE triggers for foreign-key cascades when `recursive_triggers` is
-- on -- which is not something this schema may assume about every connection
-- that ever opens it.
--
-- BEFORE, not AFTER, and that is load-bearing: the trigger body reads
-- `attachment_docs` to find the rowids to delete, and by the time an AFTER
-- trigger runs the cascade may already have removed exactly the rows it needs
-- to read. Ordering between a cascade and an AFTER trigger is not something to
-- rest an index's correctness on.
CREATE TRIGGER fts_attachments_gc BEFORE DELETE ON messages BEGIN
    DELETE FROM fts_attachments
    WHERE rowid IN (SELECT doc_id FROM attachment_docs WHERE message_id = old.id);
END;

-- Backfill from what extraction has already produced. Without this, every
-- attachment extracted before this migration is invisible to attachment search
-- until its bytes change -- and an attachment's bytes never change, so the
-- answer would be "never" (`attach::extract_attachments` skips a part whose
-- content hash and decision hash both still match, which is the whole point of
-- that skip).
-- The `EXISTS` is not belt and braces. `attachment_docs.message_id` carries a
-- foreign key, migrations run on a connection that has already enabled
-- `foreign_keys`, and this codebase treats an orphaned `index_content` row as
-- a state that happens (see `index::admin`'s own sweep: "for a database that
-- was written with `foreign_keys` off, or restored from a partial copy"). One
-- such row would fail this migration, roll it back, and leave the daemon
-- unable to open its database at all.
INSERT INTO attachment_docs (message_id, part_id)
SELECT ic.message_id, substr(ic.part, length('attachment:') + 1)
FROM index_content ic
WHERE ic.part LIKE 'attachment:%'
  AND ic.text <> ''
  AND EXISTS (SELECT 1 FROM messages m WHERE m.id = ic.message_id);

INSERT INTO fts_attachments (rowid, text)
SELECT d.doc_id, ic.text
FROM attachment_docs d
JOIN index_content ic
  ON ic.message_id = d.message_id
 AND ic.part = 'attachment:' || d.part_id;
