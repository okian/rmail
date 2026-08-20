# Practice: ask the attachment

When you need one fact out of a PDF, ask the attachment for it instead of
opening the file in another application.

## Why

The answer comes back with the passage it came from, which is the thing you
were going to go and find anyway.

## The call

```
mail attach tables <message-id> <part-id>
mail attach invoice <message-id>
mail api call AttachmentService.AskAttachment \
  '{"message_id": 42, "part_id": "2", "question": "what is the total"}'
```

{{capability:AttachmentExtractTables}} pulls tables out of a structured
document without reaching a model at all, and
{{capability:AttachmentExtractInvoice}} does the same for an invoice or a
receipt — both are what you want when the document has a shape.
{{capability:AttachmentAskAttachment}} is the one for prose: it answers over
the attachment's own text with citations into it. That last one has no mail
verb yet, so it is reached through the generic client, which is the same call
an agent makes over MCP.

Neither of the first two spends a model call unless you say so. A PDF's or an
image's tables have no structure to parse, so tables declines those two
formats rather than returning nothing, until --allow-model says the model pass
is worth paying for; invoice reads the fields the document labels itself and
takes --use-model for the rest, most usefully the line items and a vendor no
line names. The part id is optional on invoice — left off, the daemon
detects across the message's document attachments and falls back to the body.

## It only works if the text is there

Attachment text is extracted at index time. For a scanned document that means
OCR, which is off by default because it costs real CPU per attachment. A
search that finds nothing in a PDF you can plainly read is usually this. See
[[index]].

Turning it on does not ask you to pick a backend. On macOS it is Apple
Vision, which ships with the operating system and needs no setup, with a
tesseract binary on PATH as the fallback for whatever Vision errors on or
comes back empty from — and off macOS tesseract is the only backend there
is. Two limits are worth knowing before you rely on it: a scanned PDF gets its
first page rasterized and recognized and nothing past it, and off macOS not
even that, because the rasterizer is /usr/bin/sips; and an image under 4 KiB
is skipped as a signature logo or a tracking pixel rather than a page.

## And for a whole folder of them

{{capability:AttachmentExportInvoices}} turns a selection of invoices into
rows rather than answers, which is the right shape when the question is
arithmetic rather than a fact. [[find-the-clause]] is the narrow case worked
through.

## What is read out of the box, and where to change it

What is extracted and what is skipped comes from the [[config-file]]'s
index.extract table, which a fresh install runs on unchanged:

```toml
[index.extract]
attachments = true
strip_html = true
ocr = false
ocr_langs = ["eng"]
max_attachment_mb = 25
formats = ["pdf", "docx", "xlsx", "pptx", "html", "csv", "txt"]
```

That list is every format this build can read, so nothing is held back out of
the box; what it leaves out is images, and an image becomes searchable text
only through OCR. An attachment over 25 MiB is recorded too_large and one
whose format is off the list unsupported, and neither status is ever
retried — reading the same bytes the same way costs the same and changes
nothing. So raising the ceiling, or turning ocr on, is the whole of the fix:
each of those decisions is folded into the part's stored hash, so a pass
redoes exactly what your edit changed and dedups the rest away.
{{cmd:index reindex}}
asks for that pass over the open folder now rather than at the next sync, and
mail index reindex --kind extract is the same pass narrowed to the extraction
stage.

The question route has no table of its own. It draws on the ai.ask table the
mailbox-wide ask uses: claude-sonnet-5, 2000 characters of any one attachment,
an 8000-token context ceiling, a 1024-token answer. That character budget is
packed as at most six passages out of one document however high you raise it,
and a message id of 0 asks over search results instead, twelve attachments
deep. What those calls cost, and what stops them when they cost too much, is
[[ai-cost]].

Every field takes an environment override of the same shape,
RMAIL_INDEX__EXTRACT__OCR or RMAIL_AI__ASK__MAX_CHARS_PER_MESSAGE, so trying
OCR over one rebuild does not need an edit to the file.
