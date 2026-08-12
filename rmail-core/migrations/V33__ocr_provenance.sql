-- V33: OCR provenance, confidence, and recognized-text bounding boxes.
--
-- V14 is reserved by the parallel task that owns it and is not present in
-- this worktree; it lands separately. `V15` is this task's assigned number
-- (see the build brief) and must stay exactly that — do not renumber this
-- file to close the gap, and do not reuse `V14` here even temporarily.
--
-- `attachment_extractions` already says *whether* a part produced text, but
-- not *how*. A page of body text pulled straight out of a PDF's content
-- stream and a page of text a vision model guessed at from pixels carry very
-- different trust — a ranker should weigh them differently and a UI should
-- badge them differently — and a caller should not have to know that
-- `extractor` happens to start with "apple-vision" or "tesseract" to tell
-- them apart.
ALTER TABLE attachment_extractions
    ADD COLUMN provenance TEXT NOT NULL DEFAULT 'native'
    CHECK (provenance IN ('native', 'ocr'));

-- NULL for native text, where "how sure is this" has no meaning — a PDF's
-- content stream is not a guess. An OCR engine's own confidence (the mean
-- over its recognized regions) for the rest.
ALTER TABLE attachment_extractions
    ADD COLUMN confidence REAL
    CHECK (confidence IS NULL OR (confidence >= 0.0 AND confidence <= 1.0));

-- Where on the page each recognized run of text was, so a citation can point
-- at a box rather than only a byte offset: the offset says *which words*,
-- this says *where to look*. Empty for native extraction — there is no
-- geometry to recover from a content stream's text-showing operators without
-- a much heavier PDF layout engine than this crate carries.
--
-- `page` is carried from row one even though today's OCR path only ever
-- writes page 1 (an image is page 1 of itself; a scanned PDF has only its
-- first page rasterized — see `attach::ocr`'s module docs for why), so a
-- later build that OCRs every page of a scan does not need a migration to
-- grow into this table.
CREATE TABLE attachment_ocr_regions (
    message_id INTEGER NOT NULL,
    part_id    TEXT NOT NULL,
    page       INTEGER NOT NULL DEFAULT 1,
    -- Reading order within the page, top to bottom, as this crate's own sort
    -- over the backend's output determined it — neither backend promises an
    -- order on the way out.
    seq        INTEGER NOT NULL,
    text       TEXT NOT NULL,
    -- The engine's confidence for this one region. Optional: Tesseract's TSV
    -- reports it per word and Vision per recognized line, and a region here
    -- is a line either way, but a word-less line (should it ever occur) has
    -- nothing to average.
    confidence REAL,
    -- Normalized to the image, top-left origin — so a citation survives
    -- whatever the source resolution was, and a UI can scale it onto any
    -- rendered size without ever knowing the original pixel dimensions.
    x REAL NOT NULL,
    y REAL NOT NULL,
    w REAL NOT NULL,
    h REAL NOT NULL,
    PRIMARY KEY (message_id, part_id, page, seq),
    FOREIGN KEY (message_id, part_id)
        REFERENCES attachment_extractions (message_id, part_id) ON DELETE CASCADE,
    CHECK (page >= 1),
    CHECK (seq >= 0),
    CHECK (confidence IS NULL OR (confidence >= 0.0 AND confidence <= 1.0)),
    CHECK (x >= 0.0 AND x <= 1.0 AND y >= 0.0 AND y <= 1.0),
    CHECK (w >= 0.0 AND w <= 1.0 AND h >= 0.0 AND h <= 1.0)
) STRICT;

-- "What did OCR find on this attachment" — the read a citation or an
-- attachment viewer does; matches how `attachment_pages` is looked up.
CREATE INDEX idx_attachment_ocr_regions_part
    ON attachment_ocr_regions (message_id, part_id, page);
