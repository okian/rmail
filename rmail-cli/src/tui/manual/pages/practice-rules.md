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
- Backtest with {{capability:RuleBacktestRule}} over the last month, and read
  the false positives. The true positives tell you nothing you did not
  already believe.

## Keep correcting it

{{capability:RuleRecordCorrection}} records what you did instead of what the
rule did, and those corrections come back as examples. The rule improves
because you kept using the client, which is the only maintenance regime
anybody actually sustains.

## Where the ceiling is

A rule acts on new mail unattended. That is why the agent — the one thing here
that decides without a human — is off by default, has a closed vocabulary of
five reversible actions, and cannot send or delete anything. If your rule
wants to do something outside that set, it wants a human. See
[[rule-from-mistake]].
