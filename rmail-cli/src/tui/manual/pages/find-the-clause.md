# Worked example: find the clause

Somebody changed a termination clause in a contract eighteen months ago and
you cannot remember who, or which of four PDFs it was in. This is the case
search alone does badly and the case the rest of the pipeline exists for.

## Start with the filter you are sure of

```
has:attachment filename:*.pdf from:legal after:2025-01-01
```

Filters constrain; free text ranks. Getting the constraint right first turns
a mailbox-sized problem into a page-sized one, and costs nothing.

## Describe it rather than quoting it

You do not remember the wording, which is exactly when the lexical index is
the wrong tool. A leading tilde forces the semantic retriever:

```
~notice period for terminating without cause
```

## Look inside the attachments

The clause is in a PDF, not in a message body. Attachment text is indexed as
its own part, and {{capability:SearchSearchAttachments}} searches it directly.
For a scanned document, this only works if OCR was on the last time the
attachment was extracted — it is off by default because it costs real CPU per
attachment, and turning it on later is a re-extraction rather than a lost
cause. See [[index]].

## Ask the document

When you have narrowed it to a handful of files, stop searching and ask:

```
mail api call AttachmentService.AskAttachment \
  '{"message_id": 42, "part_id": "2", "question": "what is the notice period"}'
```

{{capability:AttachmentAskAttachment}} answers over that one attachment with
citations into it, so the passage that supports the answer comes back with the
answer. That is worth more here than a summary would be, because what you
actually need is the sentence. It has no mail verb of its own yet — the
generic client reaches it, and so does an agent over MCP. The part id is the
one SearchAttachments reports; a message_id of 0 asks over search results
instead of one file — the best twelve attachments for the question, out of
the box.

## Then check the answer

{{keys:ask}} over the whole mailbox is the wider version of the same move, and
[[grounded]] is why its answer is worth checking rather than trusting: follow
the citation, read the paragraph, and you are done. An answer you did not
check is a search result with extra steps.

## The numbers behind each step

Whether an attachment has text in the index at all is decided by the
[[config-file]]'s index.extract table, and a fresh install runs on these:

```toml
[index.extract]
strip_html = true
attachments = true
ocr = false
ocr_langs = ["eng"]
max_attachment_mb = 25
formats = ["pdf", "docx", "xlsx", "pptx", "html", "csv", "txt"]
```

attachments is true, so the third step above works without your having
configured anything. Three of the other fields decide whether one particular
file is findable, and none of them is a ranking question: an attachment whose
format is not in formats is recorded unsupported, one larger than 25 MiB is
recorded rather than read, and the text that is extracted is cut at two
megabytes — so a 400-page manual is searchable to its first two megabytes and
no further. A PDF you can plainly read and cannot find is usually one of
those three, or the ocr false above them.

Turning ocr on is not wasted on the mail you already have. The extraction
decision is hashed together with this table, so flipping the flag invalidates
exactly the parts OCR would now reconsider and leaves every other attachment
deduping away; {{cmd:index reindex}} over the extract stage is what walks
them. Apple Vision reads them on macOS with nothing for you to install, a
tesseract binary on PATH does it everywhere else, and ocr_langs is
priority-ordered rather than a set — eng then fra is a different
configuration from fra then eng, and swapping the two re-extracts.

Ranking treats a term found in a PDF as real evidence and weak evidence:
search.bm25_weights gives attachment text a weight of 1.0, the same as a
message body, against 8.0 for a subject. That is the arithmetic the first
step exists to sidestep, and the rest of that funnel — hybrid mode, 200
candidates a source, 25 results returned — is on [[search-vs-finder]].
{{capability:SearchSearchAttachments}} is subject to neither weighting: it
ranks attachments rather than messages, fuses its lexical and vector arms at
the same rrf_k of 60 with no field weights and no intent weights, and hands
back 20 attachments when a caller names no limit, 50 at the most.

The last step has no table of its own. {{capability:AttachmentAskAttachment}}
reads ai.ask — the same table {{keys:ask}} uses over the whole mailbox, which
[[grounded]] lists in full. Read its top_k as how many attachments and its
max_chars_per_message as how much of one: 2,000 characters against a 720-byte
passage window is two quoted windows of any one document, and six is the
ceiling however high you raise it.

Every field here takes the environment overlay in the usual shape —
RMAIL_INDEX__EXTRACT__OCR, RMAIL_SEARCH__RRF_K, RMAIL_AI__ASK__TOP_K — so a
one-off does not need an edit. How far extraction has got is a question for
{{cmd:index status}} rather than for the file: the extract stage reports its
own coverage there, counted in messages rather than in attachments, so it
tells you whether the stage is behind and never whether one particular PDF
has text yet.

## Why this order

Each step is cheaper than the one after it. Filters are free, lexical search
is nearly free, semantic search costs an embedding you have already paid for,
and only the last step calls a model. Starting at the bottom of that list is
how the AI bill grows without anybody deciding to spend anything.
