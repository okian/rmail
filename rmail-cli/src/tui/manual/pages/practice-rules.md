# Practice: write the rule after the second mistake

Do not automate a decision the first time you make it. Automate it the second
time you make the same one, and backtest before arming it.

## Why

A rule written from one example encodes the example rather than the pattern,
and the cost of a wrong rule is mail you never see.

## The order that works

- Do it by hand. Twice.
- Ask {{capability:RuleSynthesizeRule}} to propose a predicate from your
  corrections rather than writing one cold.
- Prefer the deterministic half of what it proposes. A natural language
  predicate reaches a model on every uncached evaluation; a sender or a
  subject pattern reaches nothing.
- Backtest with {{capability:RuleBacktestRule}} and read the false positives.
  The true positives tell you nothing you did not already believe. A backtest that names no window covers
  the last 30 days and reads at most 500 messages inside it, most recent
  first, so on an account busier than that the month you asked for is really
  the most recent 500 messages of it.

## Keep correcting it

{{capability:RuleRecordCorrection}} records what you did instead of what the
rule did, and the eight most recent corrections against that predicate come
back as few-shot examples on the next uncached evaluation — eight, because
every example is tokens on every such call. A correction about the very
message being classified is not an example at all: it is returned as the
answer and the model is never asked. The rule improves because you kept using
the client, which is the only maintenance regime anybody actually sustains.

## Where the ceiling is

A rule acts on new mail unattended. That is why the agent — the one thing here
that decides without a human — is off by default, has a closed vocabulary of
five reversible actions, and cannot send or delete anything. If your rule
wants to do something outside that set, it wants a human. See
[[rule-from-mistake]].

## What the engine already runs on

The rules themselves live in the database, per account, so nothing you write
is in a file. What is configuration is how the engine runs them — these
fields of the rules table in the [[config-file]], with the values they ship
with:

```toml
[rules]
enabled = true
tick_interval = "5s"
max_batch = 200
max_examples = 8
dry_run_days = 30
max_window_messages = 500
```

Every field takes an environment override of the same shape,
RMAIL_RULES__TICK_INTERVAL or RMAIL_RULES__DRY_RUN_DAYS, so a wider backtest
window for one afternoon does not need an edit. Two of these are why the
order above matters. tick_interval is the upper bound on how long after a
message arrives its rules fire, so a rule is acting on live mail five seconds
after you create it; and a rule's own enabled field defaults to true, so
there is no arming step to get to. Write enabled = false in the document if
you want one — a disabled rule is still listed and still backtestable, which
is what makes writing a rule and judging it later a thing you can do at all.

There is no confidence floor a rule has to clear before it acts on its own,
because a claude_is predicate answers yes or no and reports no score: the
backtest is the only place you get to watch it be wrong. A call that could
not be made at all — policy, a budget, an expired key — is an error and never
a no, so a rule that cannot reach the model fails loudly instead of quietly
stopping. The automation here that does have such a floor is an auto-applying
tag rule, where tags.ai.auto_apply_min_confidence ships at 0.85 and a rule's
own floor may only be stricter than that; [[practice-tags]] is where that
number lives.
