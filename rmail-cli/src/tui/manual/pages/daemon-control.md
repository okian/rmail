# Driving the daemon

Everything the background process does is reachable from the command line
without leaving the client. These are the observability verbs: what the index,
sync, the AI pipeline and the finder are doing, and the handful of controls that
change it. Each one draws its answer as a [[reports]] screen or says it on the
status line, depending on whether the answer is a table or a fact.

The four indicators on the bottom row ([[tour]]) are the same questions asked
every five seconds. When one of them turns, these are what expand it.

## The index

{{cmd:index status}} is the one to read first: a row per pipeline stage with its
coverage, what is queued for it, and what has been quarantined — then a row for
the queue itself. A stage switched off in configuration says so rather than
reporting zero coverage, because those are different states.

{{cmd:index run}} drains whatever is already queued, streaming its counters as
it goes. {{cmd:index reindex}} re-enqueues the folder you have open, which is
what you want after fixing something that made one folder's extraction wrong.
Both stream, so the report fills in while they work; Esc closes it and tells the
daemon to stop.

{{cmd:index verify}} compares the derived tables against the messages they were
derived from and reports what is adrift — the verdict first, then only the
checks with something in them. {{cmd:index gc}} reclaims rows nothing points at
any more and reports every category, zeroes included, because a category missing
because it was empty looks exactly like one this client does not know about.

{{cmd:index rebuild}} drops every derived row and re-derives it. It is the only
verb here that asks before it starts, and not because it changes things — gc
changes things and does not ask. It asks because on a large mailbox it is
minutes of work and search is degraded until it finishes. A trailing `!` skips
the question, which is what that mark means everywhere.

{{cmd:index stop}} and {{cmd:index start}} stop and start the background worker.
Stopped is a state the indicator shows, so it cannot be forgotten about.

{{cmd:index entities}} lists what extraction found of one kind — `email`,
`phone`, `amount` and the rest — with how often each appears and in how many
messages. The kind is required, because listing "everything" is a question the
RPC does not answer; get it wrong and the daemon's refusal names every kind it
knows.

## Sync

{{cmd:sync status}} is a row per folder: how many messages are stored, whether
the first walk reached the bottom of the folder, and when it last ran. The last
row is the account, and whether sync is paused for it.

{{cmd:sync now}} runs a pass over every folder and reports what each one
actually did — the strategy it chose, and how many messages arrived, changed
flags or were expunged. A folder that failed keeps its counters and says why in
the strategy column, because whatever it managed before the error is still true.

{{cmd:sync pause}} and {{cmd:sync resume}} stop and start it for this account.

## The AI pipeline

{{cmd:ai status}} answers whether the subsystem is enabled at all, whether
dispatch is paused, and what is in the queue. Those first two are different
questions: a daemon with AI switched off in configuration is not paused, and
nothing you can send it will make it start.

{{cmd:ai cost}} is the same RPC read as money: today and this month, against
whatever caps are in force. {{cmd:ai retry}} moves quarantined jobs back to
pending and says how many moved. {{cmd:ai pause}} and {{cmd:ai resume}} stop and
start dispatch.

{{cmd:ai process}} runs the pipeline over the message you are on. The report
counts what streams back rather than showing the prose — the analysis itself is
what the AI panel draws once it is cached, and two surfaces for one answer is
one too many.

## The finder

{{cmd:finder status}} reports how much the fuzzy index holds, how much is
waiting, and how much it rejected. That last figure is worth watching: a finder
quietly refusing a tenth of the mailbox looks like a finder that cannot find
things. {{cmd:finder rebuild}} builds it again from the database and says how
many entries it ended up with.

## Where to read next

- [[reports]] — the screen these draw into, and what `r` does on it.
- [[index]] — what the pipeline is actually doing, and why coverage is not one
  number.
- [[daemon]] — the process itself: starting it, where it keeps things.
