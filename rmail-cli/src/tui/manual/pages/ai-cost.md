# What the AI costs

Two AI keys sit next to each other on the message list and only one of them
spends anything. Knowing which is which is most of what you need.

- {{keys:ai.panel}} shows the cached analysis for whatever the cursor is on:
  the summary, category, priority and action items the triage pass already
  wrote at sync time. {{cmd:ai panel}} reads what is stored. It never calls a
  model, and it costs nothing.
- {{keys:ai.quick}} is the menu that can. A cached summary is still free; a
  question or a suggested reply is a model call, which is why they are behind
  a menu rather than on a bare key, and why the menu labels say which is
  which.

{{cmd:ai quick}} pins the panel to the message you aimed it at, so a folder
reloading underneath you cannot throw away an answer you have paid for.

## Where the money actually goes

Per-message triage at sync time is the recurring cost, because it is the one
that scales with how much mail you get. Everything else is per-request and
therefore bounded by how often you ask.

```
mail ai status             calls, tokens and spend so far
mail ai cost               the same, priced
mail ai budget status      caps, and how close you are
```

## Caps, and what happens at one

Budgets are per account and per work class, with a soft cap and a hard cap.
The soft cap downgrades the model — opus to sonnet to haiku — before the hard
cap blocks anything, so the first thing a budget does is get cheaper rather
than stop. Bulk and backlog work draws from a sub-share of the cap, so a
backfill cannot spend the whole day out from under interactive work.

{{capability:AiPolicySetBudget}} sets them, {{capability:AiPolicyGetSpend}}
reads them back. [[practice-budget]] is the one-sentence rule.

## Turning it off

AI can be disabled globally, per account, or per folder through a policy
rule. A per-account opt-out is a hard one: with the provider set to local,
the daemon builds no HTTP client for AI at all, so nothing in the process can
dial out for a model. See [[privacy]].

## Where to read next

- [[halve-the-ai-bill]] — the worked example.
- [[grounded]] — what a paid-for answer is worth.
