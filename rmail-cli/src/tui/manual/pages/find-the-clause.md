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
For a scanned document, this only works if OCR was enabled when it was
indexed — it is off by default because it costs real CPU per attachment. See
[[index]].

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
instead of one file.

## Then check the answer

{{keys:ask}} over the whole mailbox is the wider version of the same move, and
[[grounded]] is why its answer is worth checking rather than trusting: follow
the citation, read the paragraph, and you are done. An answer you did not
check is a search result with extra steps.

## Why this order

Each step is cheaper than the one after it. Filters are free, lexical search
is nearly free, semantic search costs an embedding you have already paid for,
and only the last step calls a model. Starting at the bottom of that list is
how the AI bill grows without anybody deciding to spend anything.
