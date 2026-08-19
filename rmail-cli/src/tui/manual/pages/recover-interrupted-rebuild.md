# Worked example: recover an interrupted rebuild

You started an index rebuild on a large mailbox, the machine slept, and now
search is returning less than it used to. Nothing is lost. The index is a
derived artifact and the message store is untouched — this is a matter of
finishing the work, not of recovering data.

## Find out what is actually missing

```
mail index status
```

Coverage is reported per kind, so the answer is usually specific: lexical
complete, entities complete, semantic at sixty percent. That tells you which
retriever is degraded, which in turn tells you what searches got worse — a
description-shaped query, in that example, and not a keyword one. See
[[index]].

## Verify before you rebuild again

```
mail index verify
```

{{capability:IndexVerify}} checks the index against the message store and
reports discrepancies rather than assuming them. Rebuilding from scratch
because status looked odd is how a twenty-minute job becomes a four-hour one.

## Drain rather than restart

Indexing work is keyed by message, kind and a hash of the content, so
re-running is a no-op for anything already done. Draining the queue picks up
exactly where it stopped:

```
mail index run
```

{{capability:IndexReindex}} is the same idea aimed at one kind, over one
selection, when only part of the corpus needs redoing.

## Only then rebuild

```
mail index rebuild
```

{{capability:IndexRebuild}} discards and reconstructs. Reach for it when
verify reports damage rather than absence, or after a change that invalidates
what was built — a different embedding model, say, whose vectors are not
comparable with the old ones.

## While it runs

Search keeps answering from whatever exists. Lexical retrieval is always on,
so mail stays findable by word throughout; what comes back is a narrower
candidate set, not an error. Pause it if it is competing with something else:

```
mail index stop
mail index start
```

## What to do differently next time

Export before a rebuild you are unsure about, and let the queue drain before
judging search quality. Those are [[practice-export]] and [[practice-index]],
and both exist because of this page.
