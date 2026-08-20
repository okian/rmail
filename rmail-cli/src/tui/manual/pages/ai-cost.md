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

Triage runs on claude-haiku-4-5, the cheapest rung, precisely because it is
the one call every message gets. Notification scoring is a second such call
on the same model, but notify.enabled is false out of the box, so switching
notifications on adds that call to triage's rather than replacing it.

The deep pass runs on claude-opus-4-8, and what earns a message one is any of
three conditions rather than all of them: a triage priority of high or above,
triage flagging the message as needing a reply, or the category appearing in
ai.deep_pass.categories — work, personal, invoice and receipt out of the box.
Personal being on that list is why more mail draws the opus pass than the word
deep suggests, and shortening the list is the cheapest change available here.

A question from the {{keys:ai.quick}} menu is priced differently again. It
runs on claude-sonnet-5 over at most 8,000 estimated context tokens, drawn
from at most 12 retrieved messages and at most 2,000 characters of each, and
returns at most 1,024 output tokens — so one question has a ceiling whatever
the size of the mailbox behind it.

```
mail ai status             calls, tokens and spend so far
mail ai cost               the same, priced
mail ai budget status      caps, and how close you are
```

## Caps, and what happens at one

Budgets are per account and per work class, with a soft cap and a hard cap.
The soft cap downgrades the model — opus to sonnet to haiku — before the hard
cap blocks anything, so the first thing a budget does is get cheaper rather
than stop. It is one rung per call and not one rung per breached cap, so
crossing the daily and monthly soft caps at once still takes opus to sonnet
rather than to haiku, and a call already on the bottom rung has nothing to
downgrade to and proceeds. Bulk and backlog work draws from a sub-share of the
cap, so a backfill cannot spend the whole day out from under interactive work.

A cap is reached at the number rather than past it — a 5.00 USD cap allows
spending up to but not including 5.00 — and that holds for the soft cap and
the hard cap alike. The two windows are UTC calendar days and months, the same
boundaries {{cmd:ai cost}} reports on, so the daily cap rolls over at UTC
midnight and not at yours.

Above that per-call enforcer sits a coarser gate, consulted once per dispatch
cycle against the global totals before any account or model is known, and what
it does at a cap is on_cap. Pause is the shipped value and it holds work back
rather than losing it — a held job is never leased that cycle, so it is still
pending when the window rolls. The alternatives are triage_only, which keeps
the cheap pass running, and drop, which terminates the held-back jobs instead
and is the only one of the three that discards work.

{{capability:AiPolicySetBudget}} sets them, {{capability:AiPolicyGetSpend}}
reads them back. [[practice-budget]] is the one-sentence rule.

## Turning it off

AI can be disabled globally, per account, or per folder through a policy
rule. The hardest of those is the daemon-wide one: with ai.provider set to
local, no HTTP client for AI generation is built anywhere in the process, so
nothing in it can dial out for a model. A per-account override or a policy
rule of local_only routes one account or one folder on-device instead, which
is a check on the dispatch path rather than a structural absence — the same
outcome for that mail, enforced one level down. See [[privacy]].

## What a fresh install is allowed to spend

AI is on out of the box and pointed at the hosted provider, so these are the
ceilings in force before you set any of your own. They live in the
[[config-file]]'s ai table:

```toml
[ai]
enabled = true
provider = "claude"
api_key_command = "security find-generic-password -s anthropic -w"

[ai.models]
triage = "claude-haiku-4-5"
deep = "claude-opus-4-8"
notify = "claude-haiku-4-5"

[ai.deep_pass]
on_priority = "high"
on_needs_reply = true
categories = ["work", "personal", "invoice", "receipt"]

[ai.ask]
model = "claude-sonnet-5"
top_k = 12
max_context_tokens = 8000
max_chars_per_message = 2000
max_tokens = 1024

[ai.limits]
max_concurrency = 4
requests_per_minute = 60
daily_token_cap = 2000000
daily_cost_cap_usd = 5.00
monthly_cost_cap_usd = 100.00
on_cap = "pause"

[ai.limits.budget]
enabled = true
soft_cap_ratio = 0.8
bulk_share = 0.5
```

Three of the numbers that govern your bill are not in that file, because they
are derived from ones that are. Nothing has stored a budget on a fresh
install, so the global hard caps are the three ai.limits ceilings verbatim;
soft_cap_ratio puts the soft cap at 4.00 USD a day and 80.00 a month, and
bulk_share puts the backlog sub-budget at half of whatever the global one
resolved to, so 2.50 USD a day. There is no monthly token cap at all —
ai.limits configures none, and one is not synthesised from the daily figure. A
per-account budget nobody has set is unlimited and bounded only by the global
one, which is why account 0, the global scope, is the cap to set first.

The one value rmail cannot supply is the key. api_key_command is a command
whose trimmed stdout is the secret, and the default reads a macOS keychain
generic password under the service name anthropic; no key is ever written into
the config file. Until that lookup returns something the daemon still starts
and the queue still fills, but every hosted call fails to authenticate, so a
fresh install with nothing in the keychain spends nothing at all.

Every field takes an environment override of the same shape,
RMAIL_AI__LIMITS__DAILY_COST_CAP_USD or
RMAIL_AI__LIMITS__BUDGET__SOFT_CAP_RATIO, double underscore for nesting. What
has been spent, and which caps this daemon is actually holding, is a question
for {{cmd:ai cost}} rather than for the file.

## Where to read next

- [[halve-the-ai-bill]] — the worked example.
- [[grounded]] — what a paid-for answer is worth.
- [[daemon-control]] — the verbs that report, pause and resume the pipeline.
