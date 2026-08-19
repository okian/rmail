# The index

Search does not read your mail; it reads three indexes built from it. All
three are derived artifacts — safe to drop and rebuild from the message
store, which is why [[recover-interrupted-rebuild]] is a recoverable
situation rather than a lost one.

- Lexical: full-text over headers, bodies, attachment text, notes and AI
  summaries. Always on, and the reason search works with no model and no
  network.
- Semantic: messages, threads and attachments chunked and embedded into a
  vector index. This is what makes a search for a description rather than a
  word work.
- Entities: people, organisations, dates, amounts, links, tracking numbers,
  order and invoice ids, extracted and cross-linked.

## It is a queue, not a pass

Sync enqueues work; indexer workers drain it in the background. Every unit is
keyed by message, index kind and a hash of the content, so re-running is a
no-op unless the content actually changed. Nothing in the UI blocks on it.

Each stage is its own kind of work, so a partial failure is partial: the
embedding provider being unreachable stops semantic indexing and leaves
lexical and entity indexing running. Search then answers with what exists.

```
mail index status      coverage per kind, and the queue depth
mail index run         drain the queue now
mail index verify      check the index against the message store
mail index gc          drop orphaned rows
```

## Why new mail is findable before it is fully indexed

Lexical indexing lands within seconds of a sync; embedding takes longer. So a
message can be findable by a word in its subject and not yet by a description
of what it is about. That is the expected shape of the gap, and
[[practice-index]] is what to do about it.

## Rebuilding

{{capability:IndexRebuild}} discards and reconstructs from the message store.
It is safe, it is resumable, and on a large mailbox it is not quick — see the
worked example. {{capability:IndexReindex}} is the narrower tool: re-run one
kind, over one selection.

## Where to read next

- [[search-vs-finder]] — what queries this substrate answers.
- [[practice-index]] — the one-sentence rule about timing.
