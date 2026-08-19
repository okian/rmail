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
  to the local ranking rather than failing the search. See [[index]].
- The finder, which is an in-memory index and never contacts IMAP at all.
- Tags, notes, and rules whose predicates are deterministic.
- Scheduling a send. Every outgoing message is queued in a local outbox and
  transmitted by a scheduler that runs regardless — see [[undo]].
- This manual, which is compiled into the binary.

## What waits

- Sync. New mail arrives when the connection does.
- Sending. An overdue message that cannot reach the SMTP server stays
  scheduled with a next attempt time and is retried with backoff. Being
  offline is never by itself a reason to mark a send failed, and nothing is
  ever dropped for being late.
- Anything that calls a hosted model, unless the local provider is
  configured. See [[ai-cost]] and [[privacy]].

## The two failure modes are not the same

A daemon that is not running and a network that is not reachable look alike
on the status line and are entirely different problems: the first takes every
local feature with it, the second takes none of them. [[daemon]] covers the
first; [[troubleshooting]] tells them apart.
