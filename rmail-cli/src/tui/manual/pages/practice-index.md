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

## Where the timing comes from

The workers field in the [[config-file]]'s index table is 4, so four workers
drain the queue in parallel and the width of the window is roughly what one
sync enqueued divided by four. It grows with the size of the sync, not with
anything you did to search.

The queue is also ordered rather than first-in. priority_recent_days is 30
and priority_mailboxes holds INBOX alone, so mail from the last thirty days
in your inbox drains ahead of everything else. That is why the gap is barely
visible on new mail and plainly visible on a five-year archive — the
archive is the part that waits.

Both take an environment override of the usual shape, RMAIL_INDEX__WORKERS
and RMAIL_INDEX__PRIORITY_RECENT_DAYS, so widening the pipeline for one
long backfill does not need an edit. How far behind you are right now is
not a config question, though: {{cmd:index status}} prints a Lag line, per
kind, in seconds between the newest message in the store and the newest one
that stage has indexed, and a dash for a stage that has indexed nothing at
all. The defaults explain the shape of the gap; that line is its current
size.
