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
the queue itself. A stage switched off in configuration reports itself off
beside its zero coverage rather than only the zero, because zero-because-off
and zero-because-behind are different states. Three of the four stages have a
switch and all three ship on — index.lexical.enabled, index.entities.enabled,
index.semantic.enabled — so a row reporting off is one somebody turned off.
Extraction has none: it is the stage the other three read from, and there is
nothing to switch.

{{cmd:index run}} drains whatever is already queued, streaming its counters as
it goes. How much it takes on per pass is index.workers, which is four out of
the box and is not a thread count — every stage writes through one SQLite
writer, so what the number sizes is the lease: four multiplies the pipeline's
sixteen into sixty-four jobs a round trip. {{cmd:index reindex}} re-enqueues
the folder you have open, which is what you want after fixing something that
made one folder's extraction wrong.
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
minutes of work and search is degraded until it finishes. A trailing ! skips
the question, which is what that mark means everywhere.

{{cmd:index stop}} and {{cmd:index start}} stop and start the background worker.
Stopped is a state the indicator shows, so it cannot be forgotten about.
index.enabled decides which state it starts in, and it is true out of the box;
setting it false means the worker starts stopped and {{cmd:index run}} still
drains on demand.

{{cmd:index entities}} lists what extraction found of one kind — email,
phone, amount and the rest — with how often each appears and in how many
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
What they suspend runs every five minutes out of the box — sync.interval while
IDLE is up, sync.poll_interval when the server offers none. Neither figure is
in the report: {{cmd:sync status}} prints when each folder last ran and not the
interval it is keeping to, so a cadence that has stopped shows up as a last-run
that stops moving rather than as a number that is wrong.

## The AI pipeline

{{cmd:ai status}} answers whether the subsystem is enabled at all, whether
dispatch is paused, and what is in the queue. Those first two are different
questions: a daemon with AI switched off in configuration is not paused, and
nothing you can send it will make it start. The shipped state is on:
ai.enabled is true and the provider is claude, so a daemon reporting not
enabled has either had that switch turned off or could not build a provider at
all — the usual reason being that ai.api_key_command produced nothing.

{{cmd:ai cost}} is the same RPC read as money: today and this month, against
whatever caps are in force — 5.00 USD a day, 100.00 a month and two million
tokens a day until you change them. All three travel in the response rather
than being read back out of your file, so what it prints is the cap this
daemon is holding. Reaching one leases nothing for the rest of the cycle
instead of downgrading or dropping work, because on_cap is pause — and that
stop is not what {{cmd:ai status}} calls paused, which is only ever a pause
somebody asked for.

{{cmd:ai retry}} moves quarantined jobs back to pending and says how many
moved. {{cmd:ai pause}} and {{cmd:ai resume}} stop and start dispatch.

{{cmd:ai process}} runs the pipeline over the message you are on. The report
counts what streams back rather than showing the prose — the analysis itself is
what the AI panel draws once it is cached, and two surfaces for one answer is
one too many.

## The finder

{{cmd:finder status}} reports how much the fuzzy index holds, how much is
waiting, and how much it rejected. That last figure is worth watching: a finder
quietly refusing a tenth of the mailbox looks like a finder that cannot find
things. What refused them is one of two caps — 200,000 entries or 25 MiB of
measured heap, whichever binds first — and entries load newest-first, so what
a full store turns away is the oldest mail. The waiting figure is the change
feed between drains, which run every 250 ms and apply at most 2,000 rows each;
persistently large means the mailbox is changing faster than that.
{{cmd:finder rebuild}} builds it again from the database and says how many
entries it ended up with.

## Where these numbers are set

None of these verbs carries a default of its own. Each reports what this
daemon resolved at startup, and it resolved that from the [[config-file]],
where a fresh install is running these:

```toml
[sync]
interval = "5m"
idle = true
poll_interval = "5m"

[index]
enabled = true
workers = 4
batch_size = 64

[ai]
enabled = true
provider = "claude"
api_key_command = "security find-generic-password -s anthropic -w"

[ai.limits]
max_concurrency = 4
requests_per_minute = 60
daily_token_cap = 2000000
daily_cost_cap_usd = 5.00
monthly_cost_cap_usd = 100.00
on_cap = "pause"

[finder]
max_entries = 200000
max_memory_mb = 25
max_drain_batch = 2000
refresh_interval_ms = 250
```

The consequential one is ai.provider. Claude is the default, so an AI pipeline
nobody has configured is one that sends message text off this machine — local
is the on-device alternative, and [[privacy]] is where that choice is laid
out. index.semantic.provider is local from the start, which is why
{{cmd:index status}} can name an embedding model — bge-small-en-v1.5, 384
dimensions — on a daemon that has never held an API key.

Every field takes an environment override of the same shape,
RMAIL_SYNC__INTERVAL or RMAIL_AI__LIMITS__DAILY_COST_CAP_USD, double
underscore for nesting. That is the reason to believe the verb over the file
when the two disagree: the file holds what you last wrote, the report holds
what this process is running.

## Where to read next

- [[reports]] — the screen these draw into, and what r does on it.
- [[index]] — what the pipeline is actually doing, and why coverage is not one
  number.
- [[daemon]] — the process itself: starting it, where it keeps things.
