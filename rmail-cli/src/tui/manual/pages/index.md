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

## The defaults, and where to change them

A fresh install runs on these, in the [[config-file]]'s index table:

```toml
[index]
enabled = true
workers = 4
batch_size = 64
priority_recent_days = 30
priority_mailboxes = ["INBOX"]

[index.semantic]
enabled = true
provider = "local"
chunk_tokens = 512
chunk_overlap = 64
```

Two of those are worth knowing rather than merely looking up.
priority_mailboxes and priority_recent_days are why new inbox mail is
searchable long before a large mailbox has finished: the queue is ordered, not
a backlog. And provider is local by default deliberately — the embedding
model runs on this machine, so turning search on does not send your mail
anywhere. See [[privacy]].

Every field takes an environment override with the same shape,
RMAIL_INDEX__WORKERS or RMAIL_INDEX__SEMANTIC__PROVIDER, so a one-off does
not need an edit. What the queue is doing *now* is a question for
{{cmd:index status}} rather than for the file.

## Where to read next

- [[search-vs-finder]] — what queries this substrate answers.
- [[practice-index]] — the one-sentence rule about timing.
- [[daemon-control]] — the verbs that drain, verify and rebuild it.
