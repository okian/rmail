# Worked example: a rule from a mistake

You have archived the same weekly report four Mondays running. That is the
signal a rule is worth writing: not "this could be automated", but "I have
already done this by hand more than twice".

## There is no rule verb yet

Rules are a daemon feature with no mail rule subcommand behind it. That is
not a gap in the engine — {{capability:RuleCreateRule}} and its siblings are
served, tested and reachable — it is a CLI surface nobody has written. Until
somebody does, the generic client is how you reach them, and it is the same
call an agent makes over MCP:

```
mail api call RuleService.ListRules '{"account_id": 1}'
```

The method may be written RuleService.ListRules, rmail.v1.RuleService.ListRules
or rmail.v1.RuleService/ListRules, and the body is proto JSON with the proto
field names.

## Say what you did, and let it propose the predicate

{{capability:RuleSynthesizeRule}} proposes a rule from a plain-English
instruction and dry-runs it, rather than asking you to write a predicate cold:

```
mail api call RuleService.SynthesizeRule \
  '{"account_id": 1, "instruction": "archive the weekly ops report", "days": 30}'
```

What comes back is a TOML document {{capability:RuleCreateRule}} would accept
verbatim, pairing deterministic predicates with an optional natural-language
one. The deterministic parts are what you should prefer: a sender, a subject
pattern, a header. A natural-language predicate reaches a model on every
uncached evaluation, so it earns its place only when nothing deterministic
separates the mail you mean from the mail you do not.

## Backtest before you arm it

{{capability:RuleBacktestRule}} runs a rule over mail that has already
arrived and reports what it would have done. It takes either a stored rule's
name or an unsaved document, which is what lets a proposal be judged before
it exists:

```
mail api call RuleService.BacktestRule \
  '{"account_id": 1, "rule_name": "weekly-ops", "days": 30}'
```

This is the step people skip, and it is the one that catches a subject
pattern that also matches the one message a month you actually need to read.
Read the false positives, not the true ones. A rule that catches 95 percent
of what you meant is a good rule; a rule that catches one thing you did not
mean is a rule you will turn off in a fortnight.

## Arm it, and keep correcting it

Create it with the TOML the synthesis produced, and from then on correcting
it is an ordinary act: {{capability:RuleRecordCorrection}} records that a
predicate answered wrongly about one message, and those corrections are
replayed as examples on later evaluations. Copy the predicate text out of the
rule rather than retyping it — the correction is matched against that exact
string, because that string is what the classification cache is keyed by.

## The bounds it runs inside

A rule's regular expressions come from you or from a model, and then run
unattended against every new message — so the pattern length, the compiled
program size and how much of a field a pattern is run over are all capped. A
counted-repetition bomb is refused when the rule is created rather than when
it matches.

## Where to read next

- [[practice-rules]] — the one-sentence version of this page.
- [[saved-vs-smart]] — when a smart folder is the better answer than a rule.
