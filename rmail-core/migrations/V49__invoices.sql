-- V49: extracted invoices/receipts, their line items, and the general
-- schema-validated extraction store (prd.md #4, #53; task 73).
--
-- # Why an extraction gets a row at all
--
-- Everything else in `extract` is computed on demand and handed straight back:
-- a table, an event, a link. An invoice is different because prd.md #53 asks
-- for a *queryable, CSV-exportable table* — "what did I spend at this vendor
-- last quarter" is a question about every invoice at once, and answering it by
-- re-reading every PDF in the mailbox would cost a model call per document per
-- question. So the extraction is persisted once and queried many times.
--
-- # Every field here is a claim, and the claim carries its origin
--
-- `provenance` is the load-bearing column. A total read off a labelled line by
-- the deterministic parser and a total a model inferred from a rendered page
-- are not the same quality of fact, and a table that stored only the number
-- would have thrown away the difference. It holds one JSON object per field:
-- which part of which document the value came from, the byte span inside that
-- part, the one-based page when the document had pages, and whether the value
-- was `parsed` or inferred by a `model`. `crate::extract::invoice::Provenance`
-- is its shape; `inferred` is the denormalized "any field here came from a
-- model", present so a caller can filter without parsing JSON.
--
-- Money is integer minor units, never a float. `_minor` columns are hundredths
-- of the row's `currency` — the same representation `index::entities` already
-- normalizes an amount to, and for the same reason: two totals a penny apart
-- must not collide, and a float cent is not a cent.
CREATE TABLE invoices (
    invoice_id  INTEGER PRIMARY KEY,
    message_id  INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    -- The MIME part the document was read from, keyed the way
    -- `crate::extract::attachment_part` keys them. Empty string means the
    -- message body itself, which is where a hosted-billing "your receipt"
    -- mail keeps its numbers.
    part_id     TEXT NOT NULL,
    -- invoice | receipt. Detected, never supplied by the caller.
    doc_kind    TEXT NOT NULL,
    -- NULL, not empty string, for a field the extraction did not find: an
    -- invoice with no vendor and an invoice whose vendor is the empty string
    -- are different statements, and only one of them is honest.
    vendor      TEXT,
    number      TEXT,
    -- ISO 4217, uppercase. NULL when no amount carried one.
    currency    TEXT,
    subtotal_minor INTEGER,
    tax_minor      INTEGER,
    total_minor    INTEGER,
    -- Unix seconds, UTC midnight of the stated day.
    issued_at   INTEGER,
    due_at      INTEGER,
    -- paid | unpaid | overdue | refunded | void. NULL when the document did
    -- not say; deliberately not defaulted to 'unpaid', which would invent a
    -- debt.
    status      TEXT,
    -- 1 when any field on this row came from a model. Denormalized from
    -- `provenance` for filtering.
    inferred    INTEGER NOT NULL DEFAULT 0,
    -- Per-field provenance, JSON. See this file's header.
    provenance  TEXT NOT NULL DEFAULT '{}',
    -- Human-readable notes the extractor attached: a subtotal+tax that does
    -- not reach the total, a model total that contradicts a parsed one. Kept
    -- because a silently reconciled number is the failure mode this whole
    -- table exists to avoid. JSON array of strings.
    warnings    TEXT NOT NULL DEFAULT '[]',
    extracted_at INTEGER NOT NULL DEFAULT (unixepoch()),
    -- Re-extracting a document replaces its row rather than accumulating
    -- revisions: the source document is immutable, so a second extraction is
    -- a better reading of the same thing, not a new fact.
    UNIQUE (message_id, part_id),
    CHECK (doc_kind IN ('invoice', 'receipt')),
    CHECK (status IS NULL OR status IN ('paid', 'unpaid', 'overdue', 'refunded', 'void')),
    CHECK (inferred IN (0, 1)),
    CHECK (currency IS NULL OR length(currency) = 3)
) STRICT;

-- "Every invoice, newest first" — the read `ExportInvoices` makes, and the one
-- `mail invoices` prints.
CREATE INDEX idx_invoices_extracted ON invoices(extracted_at DESC);

-- "What did I spend with this vendor" — the query the whole table exists for.
CREATE INDEX idx_invoices_vendor ON invoices(vendor, issued_at);

CREATE TABLE invoice_line_items (
    invoice_id  INTEGER NOT NULL REFERENCES invoices(invoice_id) ON DELETE CASCADE,
    -- Zero-based position in the document, so a client can render the lines
    -- in the order they were printed rather than in whatever order a query
    -- returns them.
    position    INTEGER NOT NULL,
    description TEXT NOT NULL,
    -- REAL because a line legitimately reads "0.5 hours". Money on the same
    -- row is still integer minor units.
    quantity    REAL,
    unit_price_minor INTEGER,
    total_minor      INTEGER,
    -- parsed | model, the same vocabulary `invoices.provenance` uses. A line
    -- read out of a spreadsheet row and a line a model recognized in prose
    -- are both useful and are not the same fact.
    origin      TEXT NOT NULL,
    PRIMARY KEY (invoice_id, position),
    CHECK (origin IN ('parsed', 'model')),
    CHECK (position >= 0)
) STRICT;

-- The general `ExtractStructured` store (prd.md #4): any message, any schema.
--
-- Separate from `invoices` on purpose. `invoices` has columns because its
-- shape is fixed and people query across it; this table's shape is whatever
-- schema the caller named, so the payload is JSON that has already been
-- validated against that schema by `crate::extract::structured::validate`
-- before it was written. A row here is therefore a document that *is* valid
-- for the schema it names — not a document somebody hopes is.
CREATE TABLE structured_extractions (
    extraction_id INTEGER PRIMARY KEY,
    message_id  INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    -- A built-in schema's name (`invoice`, `flight`, ...), or `custom` for a
    -- schema the caller supplied.
    schema_name TEXT NOT NULL,
    -- SHA-256 of the canonical schema JSON. Part of the key so re-extracting
    -- with a *changed* custom schema stores a second document rather than
    -- silently overwriting a record that was valid for a different shape.
    schema_hash TEXT NOT NULL,
    -- The validated document.
    data        TEXT NOT NULL,
    -- The model this daemon was *configured* to extract with, not necessarily
    -- the one that answered: the budget enforcer may downgrade a request under
    -- a soft cap, and `ai_ledger` — keyed by the same message — is the
    -- authority for what actually ran. Recorded anyway because a document
    -- extracted by haiku and one extracted by opus are different-quality
    -- readings and a reader is entitled to know which was asked for.
    model       TEXT NOT NULL,
    created_at  INTEGER NOT NULL DEFAULT (unixepoch()),
    UNIQUE (message_id, schema_name, schema_hash),
    CHECK (schema_name <> ''),
    CHECK (data <> '')
) STRICT;

CREATE INDEX idx_structured_extractions_schema
    ON structured_extractions(schema_name, created_at DESC);
