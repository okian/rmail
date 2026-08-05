-- V10: the entity graph.
--
-- Three tables because an entity, a sighting of it, and a relationship between
-- two of them are different things with different cardinalities.
--
-- `entities` is the canonical set: one row per distinct thing, keyed by
-- `(kind, norm)`. The normalized form is what makes that work -- `Ada@Example.COM`
-- and `ada@example.com` are one address, `+1 (555) 010-1234` and `+15550101234`
-- are one phone. Keeping the raw `value` alongside it means a UI can show what
-- was actually written while search matches on what it means.
--
-- `entity_mentions` is where each one was seen, with spans, so a result can be
-- highlighted without re-running the extractors over the body. The primary key
-- is `(entity, message, part, span_start)`: the same address appearing twice in
-- one body is two mentions, and re-extracting the same message must overwrite
-- them rather than accumulate.
--
-- `entity_edges` is co-occurrence: two entities seen in the same message are
-- related, and the weight is how often. It is deliberately not derived on
-- demand -- "who else was on the threads about this invoice number" is a graph
-- walk, and a walk over a join of mentions would read the whole table.
CREATE TABLE entities (
    entity_id INTEGER PRIMARY KEY,
    -- email | phone | url | amount | date | tracking_no | order_id
    -- | invoice_id | iban | person | org | address
    kind      TEXT NOT NULL,
    -- As written, for display.
    value     TEXT NOT NULL,
    -- Canonical form, for identity and lookup.
    norm      TEXT NOT NULL,
    -- Kind-specific detail as JSON (currency for an amount, carrier for a
    -- tracking number). Kinds gain fields over time and a column per kind
    -- would be unreadable long before it was wrong.
    meta      TEXT,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    UNIQUE (kind, norm),
    CHECK (kind IN ('email', 'phone', 'url', 'amount', 'date', 'tracking_no',
                    'order_id', 'invoice_id', 'iban', 'person', 'org', 'address')),
    CHECK (norm <> '')
) STRICT;

CREATE TABLE entity_mentions (
    entity_id  INTEGER NOT NULL REFERENCES entities(entity_id) ON DELETE CASCADE,
    message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    -- Which `index_content` part it was found in.
    part       TEXT NOT NULL,
    -- Byte offsets into that part's normalized text, for highlighting.
    span_start INTEGER NOT NULL,
    span_end   INTEGER NOT NULL,
    -- Which extractor found it, so a regex hit can be told from a model's.
    source     TEXT NOT NULL,
    confidence REAL NOT NULL DEFAULT 1.0,
    PRIMARY KEY (entity_id, message_id, part, span_start)
) STRICT;

-- "What was found in this message" -- the read a re-extraction and a result
-- renderer both do.
CREATE INDEX idx_entity_mentions_message ON entity_mentions(message_id);

CREATE TABLE entity_edges (
    src_id INTEGER NOT NULL REFERENCES entities(entity_id) ON DELETE CASCADE,
    dst_id INTEGER NOT NULL REFERENCES entities(entity_id) ON DELETE CASCADE,
    -- Relationship kind; `co_occurs` is the only one the regex stage produces.
    rel    TEXT NOT NULL,
    weight REAL NOT NULL DEFAULT 1.0,
    PRIMARY KEY (src_id, dst_id, rel),
    CHECK (rel IN ('co_occurs')),
    -- The direction invariant, in the schema rather than in a comment. The
    -- pair is undirected and stored once, low id first, so that "who else
    -- appears with this invoice number" does not depend on which end you start
    -- from. A writer that got the order wrong used to produce a mirrored row
    -- that read as a separate relationship and silently doubled a weight;
    -- here it is a transaction failure on the first attempt.
    CHECK (src_id < dst_id),
    CHECK (weight > 0.0)
) STRICT;

-- Walks start from either end.
CREATE INDEX idx_entity_edges_dst ON entity_edges(dst_id, rel);
