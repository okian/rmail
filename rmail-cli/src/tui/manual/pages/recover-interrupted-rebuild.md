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
description-shaped query, in that example, and not a keyword one. The same
report carries the queue depth and the quarantined count, which is what
separates a stage that is behind from a stage that has stopped: pending work
means the answer is to let the drain finish. See [[index]].

## Verify before you rebuild again

```
mail index verify
```

{{capability:IndexVerify}} checks the index against the message store and
reports discrepancies rather than assuming them. Rebuilding from scratch
because status looked odd is how a twenty-minute job becomes a four-hour one.
It takes no arguments and it changes nothing — it never repairs, enqueues or
deletes — so it is safe against a running daemon, though on a large mailbox
the reconciliation walks every recorded row against every stored one and
takes minutes. Read its content-hash drift beside the pending count from
status, because a queue with work still in it looks like drift: a note added
a minute ago changes that message's hash, and the lexical job to catch up is
still pending.

## Drain rather than restart

Indexing work is keyed by message, kind and a hash of the content, so
re-running is a no-op for anything already done. Draining the queue picks up
exactly where it stopped:

```
mail index run
```

It drains until the queue is empty — that is what its max-jobs default of
zero means — and reports a progress frame per batch of sixteen jobs, so you
can watch it move; it runs even while the background worker is stopped. A
stage that keeps failing does not hold up the ones behind it: a failed job
waits thirty seconds, doubling per attempt to a thirty-minute ceiling, and
after five attempts it is quarantined as dead, visible for diagnosis and
invisible to workers. That quarantined count is why coverage can stop
climbing short of complete with nothing else wrong.

{{capability:IndexReindex}} is the same idea aimed at one kind, over one
selection, when only part of the corpus needs redoing.

## Only then rebuild

```
mail index rebuild --all
```

{{capability:IndexRebuild}} discards and reconstructs. Reach for it when
verify reports damage rather than absence, or after a change that invalidates
what was built — a different embedding model, say, whose vectors are not
comparable with the old ones. Out of the box that model is the local
bge-small-en-v1.5 at 384 dimensions, so this is a case you reach only after
you change it. There is no default scope: the command refuses without --all
or at least one --kind, because defaulting a wipe to everything is the
accident that guard exists to prevent, and on a terminal it asks before it
deletes anything. Pass -y for a script, which has no terminal to answer on
and is refused rather than assumed to have meant yes.

## While it runs

Search keeps answering from whatever exists. Lexical retrieval is always on,
so mail stays findable by word throughout; what comes back is a narrower
candidate set, not an error. Pause it if it is competing with something else:

```
mail index stop
mail index start
```

Stopping is an operator's "not right now" and not a durable policy. The
queued work is durable, but the pause is held in memory only, so a restarted
daemon comes back indexing whichever state index.enabled names — true out of
the box — whether or not you stopped it before.

## The numbers this example rests on

A fresh install runs the index on three values, in the [[config-file]]'s
index table:

```toml
[index]
enabled = true
workers = 4
batch_size = 64
```

workers is not a thread count: every stage writes through one SQLite writer,
so what the number sizes is the lease — four multiplies the pipeline's
sixteen into sixty-four jobs a pass. An idle queue is looked at every two
seconds; a pass that fills its lease does not wait at all, which is why a
backlog drains faster than that interval suggests. batch_size is chunks per
embed request. Both take an environment override with the usual shape,
RMAIL_INDEX__WORKERS, so a one-off does not need an edit.

The queue's own numbers are fixed, and no config key moves them: a leased job
is held for five minutes, and a worker that dies leaves its lease to expire
rather than losing the job. There is no resume position to lose either —
nothing records how far a rebuild got, because what has been done is recorded
per message and per kind and every enqueue is compared against that.

{{cmd:index verify}} and {{cmd:index gc}} both run with no arguments. The
second deletes only rows whose parent is already gone — an entity nothing
mentions, a vector whose chunk was removed, a full-text row for deleted mail
— and sweeps the search caches back to their configured bounds while it is
there. Its one flag is the exception you have to ask for: --purge-caches drops
compiled query plans too, and each of those is a paid Claude call that will
be paid again. The rest of these verbs, and what each indicator on the
bottom row expands into, are in [[daemon-control]].

## What to do differently next time

Export before a rebuild you are unsure about, and let the queue drain before
judging search quality. Those are [[practice-export]] and [[practice-index]],
and both exist because of this page.
