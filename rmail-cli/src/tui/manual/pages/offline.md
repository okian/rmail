# Working offline

Your mail is already on this machine. Everything that reads it keeps working
with the network unplugged, because reading mail here is a database query and
not a protocol exchange.

## What keeps working

- Reading. The message list, the viewer and every flag you set are local
  database operations. Nothing blocks on IMAP.
- Search, in full. Retrieval and ranking run against the local index, so a
  ranked search offline is the same search rather than a degraded one — with
  the exception of a reranker configured to reach a model, which falls back
  to the local ranking rather than failing the search. search.rerank is auto
  out of the box, which means the local cross-encoder for an interactive
  search and Claude only for an explicit deep search, so it is the deep
  search that loses its last stage with the network gone. The field also
  takes off and cross_encoder, either of which keeps every stage local.
  See [[index]].
- The finder, which is an in-memory index and never contacts IMAP at all.
- Tags, notes, and rules whose predicates are deterministic.
- Scheduling a send. Every outgoing message is queued in a local outbox and
  transmitted by a scheduler that runs regardless — see [[undo]].
- This manual, which is compiled into the binary.

## What waits

- Sync. New mail arrives when the connection does. sync.idle is true by
  default, so the server pushes as mail lands, and sync.interval — five
  minutes — is both the poll that runs alongside that push and the cadence
  at which IDLE is torn down and reissued; where the server offers no IDLE,
  sync.poll_interval, also five minutes, is the whole of the cadence. Either
  way a connection that comes back while you are looking elsewhere is noticed
  within five minutes, and {{cmd:sync now}} is how you decline to wait for it.
- Sending. An overdue message that cannot reach the SMTP server stays
  scheduled with a next attempt time and is retried with backoff, the delay
  starting at thirty seconds and doubling. Being offline is classified
  transient rather than permanent, so it is never by itself a reason to mark
  a send failed, and nothing is ever dropped for being late. What is finite
  is the attempt budget: send.max_retries is five, and the four delays it
  contains are thirty seconds, one minute, two minutes and four minutes, so
  seven and a half minutes of being unreachable is enough to spend it. An
  outage lasting an afternoon therefore leaves a failed row waiting for you
  rather than one still trying, and returning that row to the queue is a
  deliberate act — see [[undo]]. The thirty-minute ceiling on the backoff is
  never reached inside five attempts; it starts to matter only if you raise
  send.max_retries to eight or more.
- Anything that calls a hosted model, unless the local provider is
  configured. ai.provider is claude by default, so out of the box this is
  the part of the AI surface that stops rather than degrades. See
  [[ai-cost]] and [[privacy]].

## Where the waiting times come from

Both sync cadences live in the [[config-file]]'s sync table, and a fresh
install runs on these:

```toml
[sync]
interval = "5m"
idle = true
qresync = true
poll_interval = "5m"
```

The two intervals are the same five minutes, so which of them applies — a
decision the server makes rather than you — does not change how far behind
new mail can be. Each field takes an environment override of the same shape,
RMAIL_SYNC__INTERVAL or RMAIL_SYNC__POLL_INTERVAL, so shortening the gap for
one trip does not need an edit to the file.

The send table the retry numbers come from is set out in full on [[undo]],
where the same fields decide the undo window. Where sync actually stands —
per folder, with the time of its last pass and whether it is paused for the
account — is a question for {{cmd:sync status}} rather than for either file.

## The two failure modes are not the same

A daemon that is not running and a network that is not reachable look alike
on the status line and are entirely different problems: the first takes every
local feature with it, the second takes none of them. [[daemon]] covers the
first; [[troubleshooting]] tells them apart.
