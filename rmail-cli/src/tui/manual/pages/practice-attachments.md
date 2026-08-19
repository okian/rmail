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

## It only works if the text is there

Attachment text is extracted at index time. For a scanned document that means
OCR, which is off by default because it costs real CPU per attachment. A
search that finds nothing in a PDF you can plainly read is usually this. See
[[index]].

## And for a whole folder of them

{{capability:AttachmentExportInvoices}} turns a selection of invoices into
rows rather than answers, which is the right shape when the question is
arithmetic rather than a fact. [[find-the-clause]] is the narrow case worked
through.
