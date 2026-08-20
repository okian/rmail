# Worked example: halve the AI bill

Spend has crept up. This is the order to look at things in, cheapest fix
first.

## Find out where it went

```
mail ai cost                 requests, tokens and dollars for the day
mail ai budget status        caps, and how close you are to them
```

Almost always the answer is per-message triage at sync time. It is the only AI
cost that scales with how much mail arrives rather than with how often you ask
for something, so it dominates as soon as the mailbox is busy.

{{cmd:ai cost}} prices one window at a time — today, or the calendar month
with --month — as requests, input tokens, output tokens and dollars, and it
prints no caps. {{cmd:ai status}} is the one that puts today's spend beside the
three caps in force, and adds the queue depth and the pause state. Ask them,
not the config file: the file holds the ceiling and knows nothing about what
has been spent against it. Neither breaks the total down by model; the split
by class of work is in mail ai budget status, and the model, the pass and the
cost of every single call are in the audit ledger,
{{capability:AuditQueryAiCalls}}.

## Downgrade the model that runs most often

Triage is a per-message classification over text already in front of the
model. That is haiku work. The deep pass, which runs on far fewer messages, is
where a larger model earns its cost.

```
[ai.models]
triage = "claude-haiku-4-5"
deep = "claude-opus-4-8"
```

Both of those lines are the defaults, so on a config nobody has edited this
step is a check rather than a change — if triage still reads claude-haiku-4-5
and the daily figure is high, the model is not what is costing you and the
next two sections are. Look anyway, because a raised triage model is the one
edit that multiplies by every message that arrives.

## Narrow what gets processed at all

An account you never ask questions about does not need enrichment. A per
account opt-out is a real off switch rather than a filter:

```
[[accounts]]
name = "Personal-Legal"
[accounts.ai]
enabled = false
```

Every account starts enrolled — accounts.ai.enabled is true when the block is
absent — so this is a list you write, not one you trim.

A folder-level policy rule does the same job at a finer grain, and a rule of
local-only routes that folder to the on-device path instead of turning the
feature off. See [[privacy]].

## Set the cap you should have set first

```
mail ai budget set --account 1 --daily-soft-usd 1.60 --daily-hard-usd 2.00
```

The soft cap downgrades the model before the hard cap blocks anything, so the
first thing a budget does is make calls cheaper rather than stop them.
Backlog work draws from a sub-share — half of the scope's caps unless you say
otherwise — so a backfill cannot spend the day out from under interactive
work.

You are not moving off nothing. With no budget stored, the global ceilings
come from ai.limits: 5.00 dollars a day, 100.00 a month, two million tokens a
day, and reaching one pauses AI dispatch rather than dropping work silently,
because on_cap is pause. The soft cap of 1.60 above is also not a number you
have to invent — a cap a stored budget leaves unset takes its soft cap at 0.8
of the hard one, which is exactly 1.60 against 2.00.

A cap you leave off that command line is left uncapped rather than set to
zero, and one set replaces the whole stored row for that scope, so pass every
cap you want in force and not only the one you are changing.

## Move embeddings on-device

Embedding every message is the other recurring cost, and out of the box it is
already nothing: index.semantic.provider is local, so the embedder runs on
this machine.

```
[index.semantic]
provider = "local"
```

Read that line before you write it. It is a fix only if somebody moved the
provider to voyage, which is the hosted backend and the only one of the three
that bills — none, the third, turns semantic indexing off rather than moving
it. Local costs weights instead of dollars — bge-small-en-v1.5 at 384
dimensions — and allow_download is false by default, so the daemon will not
fetch them for you; turn it on for one provisioning run, or fill the model
cache directory out of band.

## Where the numbers come from

Nothing above is a figure this page invented. A fresh install runs on these,
in the [[config-file]]'s ai table:

```toml
[ai]
enabled = true
provider = "claude"

[ai.models]
triage = "claude-haiku-4-5"
deep = "claude-opus-4-8"
notify = "claude-haiku-4-5"

[ai.limits]
daily_cost_cap_usd = 5.00
monthly_cost_cap_usd = 100.00
daily_token_cap = 2000000
on_cap = "pause"

[ai.limits.budget]
enabled = true
soft_cap_ratio = 0.8
bulk_share = 0.5
```

Two of those are not what they look like. provider is claude, the only
backend that can leave this machine, and it is daemon-wide — one account moves
with {{capability:AiPolicySetAiProvider}}, and
{{capability:AiPolicyGetAiProvider}} reports which backend an account is on
and whether its local model is ready. And notify is a second haiku call that
would run on every synced message, except that notify.enabled is false out of
the box; turning notifications on adds that call to triage's rather than
replacing it.

Every field here takes an environment override of the same shape,
RMAIL_AI__PROVIDER or RMAIL_AI__LIMITS__DAILY_COST_CAP_USD, so a one-off does
not need an edit. Stored budgets are the exception, and deliberately so: they
live in the daemon's database, {{capability:AiPolicySetBudget}} is the only
thing that writes them, and {{capability:AiPolicyGetSpend}} is what reads
them and the spend against them back.

## What not to do

Do not turn off the AI panel to save money. {{keys:ai.panel}} reads what has
already been paid for; hiding it changes nothing about the bill and costs you
the only view of what the spend bought. See [[ai-cost]].
