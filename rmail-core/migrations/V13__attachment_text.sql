-- V13: what happened to each attachment, and where its pages are.
--
-- The text itself goes to `index_content` as `attachment:<part_id>`, alongside
-- the body and the subject, because a ranker should not care which of them a
-- term came from beyond the weight it carries. What does not fit there is the
-- bookkeeping: whether extraction succeeded, what it was tried with, and how to
-- turn a byte offset back into a page number.
--
-- `attachment_extractions` exists mainly so that *failure* is recorded. An
-- encrypted PDF, a format nothing here reads, an attachment past the size
-- limit: each of those legitimately produces no text, and without a row saying
-- so they are indistinguishable from "not extracted yet". The pipeline would
-- then retry them on every pass, for ever, at whatever they cost.
CREATE TABLE attachment_extractions (
    message_id   INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    -- The MIME part id, matching `attachments.part_id`.
    part_id      TEXT NOT NULL,
    -- ok | empty | too_large | unsupported | encrypted | failed | timeout
    --
    -- `empty` and `failed` are different facts: a scanned PDF with no text
    -- layer is a candidate for OCR, an extractor that fell over is a bug.
    status       TEXT NOT NULL,
    -- Which extractor ran, so a result can be attributed and so a later build
    -- with a better one can re-run only what it would improve.
    extractor    TEXT NOT NULL,
    -- SHA-256 of the attachment's decoded bytes. What makes this idempotent:
    -- the queue redelivers on lease expiry, and re-running a PDF parse over
    -- unchanged bytes is the most expensive no-op in the indexer.
    content_hash BLOB NOT NULL,
    -- Decoded size, so `max_attachment_mb` decisions are auditable after the
    -- fact rather than only visible in a log line that has since rotated.
    bytes        INTEGER NOT NULL,
    -- Extracted characters, and pages if the format has them.
    chars        INTEGER NOT NULL DEFAULT 0,
    pages        INTEGER,
    extracted_at INTEGER NOT NULL DEFAULT (unixepoch()),
    PRIMARY KEY (message_id, part_id),
    CHECK (status IN ('ok', 'empty', 'too_large', 'unsupported', 'encrypted',
                      'failed', 'timeout')),
    CHECK (extractor <> ''),
    CHECK (bytes >= 0),
    CHECK (chars >= 0)
) STRICT;

-- "What still needs extracting" and "what did this build fail on" — the two
-- reads that drive the pipeline and its repair.
CREATE INDEX idx_attachment_extractions_status
    ON attachment_extractions(status, extractor);

-- Byte offsets of each page within the extracted text.
--
-- A citation into a fifty-page contract has to say *page 31*, and the only way
-- to get there from a search hit — which knows a byte offset into
-- `index_content` — is a table like this. Offsets rather than a copy of the
-- page's text, so there is exactly one copy of the words and no way for the two
-- to disagree.
CREATE TABLE attachment_pages (
    message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    part_id    TEXT NOT NULL,
    -- One-based, as a reader counts them.
    page       INTEGER NOT NULL,
    -- Half-open: `span_start` is the first byte of the page and `span_end` is
    -- one past its last. An inclusive-inclusive lookup resolves every exact
    -- boundary to the *earlier* page, which is wrong for precisely the offsets
    -- a citation is most likely to carry.
    span_start INTEGER NOT NULL,
    span_end   INTEGER NOT NULL,
    PRIMARY KEY (message_id, part_id, page),
    CHECK (page >= 1),
    CHECK (span_end >= span_start)
) STRICT;
