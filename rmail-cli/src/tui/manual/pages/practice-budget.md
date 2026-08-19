# Practice: set the cap before turning AI on

Set a daily cost cap in the same sitting in which you enable AI, not in the
sitting after the first surprising invoice.

## Why

The only AI cost that scales with how much mail you receive is the one that
runs before you ask for anything, so the bill grows without anybody deciding
to spend.

## The two lines

```
mail ai budget set --account 1 --daily-soft-usd 1.60 --daily-hard-usd 2.00
mail ai budget status --account 1
```

{{capability:AiPolicySetBudget}} stores the caps;
{{capability:AiPolicyGetSpend}} reads spend back. The soft cap is a separate
number rather than a fraction of the hard one, and it downgrades the model
instead of blocking — so the first thing a budget does is get cheaper.

Account 0 is not an account. It is the global budget every call counts
toward, and it is the default, so a bare set with no --account is the
mailbox-wide cap.

## Sub-budget the bulk work

Backlog and backfill work draws from a share of the cap rather than the whole
of it. That is what stops a one-off reprocessing job from spending the day's
budget out from under the summary you asked for at four in the afternoon.

## Then look at where it went

[[halve-the-ai-bill]] is the diagnosis in order. The short version is that the
model doing triage matters more than the model doing anything else, because it
runs on every message.
