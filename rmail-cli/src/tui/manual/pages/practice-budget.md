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

Pass every cap you want in force, not only the one you are changing: a set
replaces whatever was stored for that scope, and a cap left off the line
is left uncapped rather than zeroed. The boundary is at or above, so a
--daily-hard-usd of 0 forbids all spending while omitting it forbids none.

## Sub-budget the bulk work

Backlog and backfill work draws from a share of the cap rather than the whole
of it. That is what stops a one-off reprocessing job from spending the day's
budget out from under the summary you asked for at four in the afternoon.

That share is half out of the box — ai.limits.budget.bulk_share is 0.5, so
the backlog's day on an untouched install is 2.50 USD of the global 5.00. A
job is charged as bulk at queue priority 500 or beyond, which is where
backfill sits and where normal and recent work does not, and --bulk on the
set line budgets that sub-share explicitly. A bulk call is then checked
against both.

## What the cap already is

The sitting this page names is not one in which you switch anything on.
ai.enabled is true out of the box and ai.provider is claude, so setting a
budget tightens ceilings that already exist rather than creating the first
one. The [[config-file]]'s ai.limits table ships daily_cost_cap_usd 5.00,
monthly_cost_cap_usd 100.00, daily_token_cap 2,000,000 and on_cap pause,
with max_concurrency 4 and requests_per_minute 60 bounding the rate rather
than the spend. Nothing has stored a budget on a fresh install, so those
three caps are the global hard caps verbatim, and
ai.limits.budget.soft_cap_ratio, 0.8, derives the soft caps from them: 4.00
USD a day and 80.00 a month.

What keeps an untouched install from spending any of that is the key rather
than the caps. api_key_command reads a macOS keychain generic password
under the service name anthropic by default, and until that lookup returns
something every hosted call fails to authenticate — so the day you put the
key in is the day the 5.00 starts applying, and that is the sitting this
page is about.

Every field takes an environment override of the same shape,
RMAIL_AI__LIMITS__DAILY_COST_CAP_USD or
RMAIL_AI__LIMITS__BUDGET__SOFT_CAP_RATIO, double underscore for nesting, so
holding a tighter cap for one afternoon does not need an edit. What has been
spent against whichever caps are in force is a question for {{cmd:ai cost}}
rather than for the file, and [[ai-cost]] is where each of these numbers
comes from.

## Then look at where it went

[[halve-the-ai-bill]] is the diagnosis in order. The short version is that the
model doing triage matters more than the model doing anything else, because it
runs on every message.
