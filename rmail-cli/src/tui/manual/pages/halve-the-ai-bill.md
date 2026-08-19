# Worked example: halve the AI bill

Spend has crept up. This is the order to look at things in, cheapest fix
first.

## Find out where it went

```
mail ai cost                 spend, by model and by class of work
mail ai budget status        caps, and how close you are to them
```

Almost always the answer is per-message triage at sync time. It is the only AI
cost that scales with how much mail arrives rather than with how often you ask
for something, so it dominates as soon as the mailbox is busy.

## Downgrade the model that runs most often

Triage is a per-message classification over text already in front of the
model. That is haiku work. The deep pass, which runs on far fewer messages, is
where a larger model earns its cost.

```
[ai.models]
triage = "claude-haiku-4-5"
deep = "claude-opus-4-8"
```

## Narrow what gets processed at all

An account you never ask questions about does not need enrichment. A per
account opt-out is a real off switch rather than a filter:

```
[[accounts]]
name = "Personal-Legal"
[accounts.ai]
enabled = false
```

A folder-level policy rule does the same job at a finer grain, and a rule of
local-only routes that folder to the on-device path instead of turning the
feature off. See [[privacy]].

## Set the cap you should have set first

```
mail ai budget set --account 1 --daily-soft-usd 1.60 --daily-hard-usd 2.00
```

The soft cap downgrades the model before the hard cap blocks anything, so the
first thing a budget does is make calls cheaper rather than stop them.
Backlog work draws from a sub-share, so a backfill cannot spend the day out
from under interactive work.

## Move embeddings on-device

Embedding every message is the other recurring cost. The local provider ends
it outright, at the price of provisioning weights once:

```
[index.semantic]
provider = "local"
```

## What not to do

Do not turn off the AI panel to save money. {{keys:ai.panel}} reads what has
already been paid for; hiding it changes nothing about the bill and costs you
the only view of what the spend bought. See [[ai-cost]].
