-- V9: the lexical index.
--
-- Contentless (`content=''`): FTS5 stores the inverted index and nothing else.
-- The text already lives in `index_content`, and a second copy would double the
-- largest table in the database to buy a snippet feature that can read the
-- original just as easily.
--
-- `contentless_delete=1` is what makes that survivable. A plain contentless
-- table cannot delete a row without being handed the original column values
-- back -- which would mean keeping the text after all, or leaving deleted mail
-- in the index forever. This flag lets `DELETE ... WHERE rowid = ?` work, at
-- the cost of a little extra bookkeeping per row.
--
-- The columns are the *ranking* fields, not the storage parts. `sender` and
-- `recipients` are separate because mail from someone is a stronger match than
-- mail merely addressed to them alongside forty other people, and the PRD
-- weights them 4.0 against 2.0. `rowid` is `messages.id`, so a hit joins
-- straight back to the message with no mapping table.
--
-- `remove_diacritics 2` folds accents in the *index*, so a query for "cafe"
-- finds "café". It is the version that handles combining marks correctly;
-- version 1 misses several. Extraction already applies NFC, so the two agree
-- on what a character is before the tokenizer sees it.
CREATE VIRTUAL TABLE fts_messages USING fts5(
    subject,
    sender,
    recipients,
    body,
    attachments,
    notes,
    summary,
    content = '',
    contentless_delete = 1,
    tokenize = "unicode61 remove_diacritics 2"
);

-- A deleted message must leave the index with it. `messages` cascades to
-- `index_content`, but a virtual table takes no foreign key, so the cascade has
-- to be spelled out. Without it, deleted mail stays searchable and every hit on
-- it dangles.
CREATE TRIGGER fts_messages_gc AFTER DELETE ON messages BEGIN
    DELETE FROM fts_messages WHERE rowid = old.id;
END;
