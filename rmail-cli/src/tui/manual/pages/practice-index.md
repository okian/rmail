# Practice: let the index catch up

After a large sync or a rebuild, check index status before concluding that
search has got worse.

## Why

Lexical indexing lands within seconds and embedding takes longer, so there is
a real window in which a message is findable by a word in it and not yet by a
description of it.

## What to look at

```
mail index status
```

Coverage is per kind. Lexical complete and semantic at sixty percent is not a
broken index; it is a queue with work left in it, and it tells you exactly
which searches are currently degraded — the description-shaped ones.

## Do not rebuild because status looked odd

Verify first. Draining the queue resumes exactly where it stopped, because
every unit of work is keyed by content hash and re-running is a no-op.
Rebuilding starts over. [[recover-interrupted-rebuild]] is that decision
worked through.

## And do not block on it

Nothing in the UI waits for indexing, so there is never a reason to sit and
watch it. Sync enqueues, workers drain, search answers from whatever exists.
See [[index]].
