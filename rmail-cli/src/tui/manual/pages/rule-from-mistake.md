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

Both calls read days the same way, and both let you leave it out: a zero or
absent days takes the rules table's dry_run_days, thirty, and either way the
window materializes at most max_window_messages messages — five hundred, most
recent first — so a backtest over a busy year reads the newest five hundred
rather than every message in it.

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

The replay is bounded, and the bound is worth knowing: the most recent
max_examples corrections for that predicate — eight — ride along on every
uncached classification, and each one is tokens on every one of those calls,
which is why the number is small rather than generous. A correction recorded
against the message in front of you is stronger than an example. That message
is answered from the correction and never reaches a model again.

## The bounds it runs inside

A rule's regular expressions come from you or from a model, and then run
unattended against every new message — so the pattern length, the compiled
program size and how much of a field a pattern is run over are all capped. A
counted-repetition bomb is refused when the rule is created rather than when
it matches.

Out of the box that is 512 bytes of regex source, a 262144-byte compiled
program, and the first 65536 characters of any one field a pattern is run
over. All three are configurable and all three are floored — at 64 bytes,
4096 bytes and 1024 characters — so a zero typed into one of them refuses
nothing rather than refusing every pattern in the daemon.

Three further bounds have no knob at all: a rule name is at most 64
characters, a claude_is predicate at most 500, and a whole rule document at
most 16384 bytes. The 500 is the one you will meet. That text is sent to a
model on every uncached classification, so a predicate written as a paragraph
is a paragraph you pay for on every message it is asked about.

## Where the engine's numbers live

Your rules are not in the config file, and looking for them there is the first
wrong turn. A rule is a database row, one per account, holding the document
verbatim under a name unique per account case-insensitively — so
{{capability:RuleListRules}} is how you read one back, and there is no file to
diff. What the [[config-file]] carries is only the knobs that govern how the
engine runs them:

```toml
[rules]
enabled = true
tick_interval = "5s"
max_batch = 200
archive_mailbox = "Archive"
max_pattern_len = 512
regex_size_limit_bytes = 262144
max_match_chars = 65536
max_examples = 8
max_window_messages = 500
dry_run_days = 30
```

Three of those are worth knowing rather than looking up. tick_interval is the
upper bound on how long after a message arrives its rules fire: five seconds,
because the evaluator re-reads the event log on a tick rather than being
called by the sync. max_batch stops one tick at two hundred messages, so an
initial sync landing thousands is not one unbounded evaluation pass. And
enabled gates only that automatic path — turn it off and RuleService still
creates, lists, backtests and evaluates on request, which is the difference
between disarming the engine and losing it.

archive_mailbox is the mailbox an archive = true action moves to. A rule that
says archive names no destination itself, which is what lets the same rule
document be right on a provider that calls that folder something else.

The other thing here called a rule is not this engine at all. A tag rule,
written with mail tag-rules set, decides whether a confident AI tag suggestion
may apply itself. Its mode is suggest unless you say auto, so nothing applies
itself out of the box; the per-rule floor that command sets defaults to 0.9;
and no tag rule can go below the mailbox-wide
tags.ai.auto_apply_min_confidence of 0.85, because the effective floor is the
higher of the two. See [[practice-tags]].

Every field takes an environment override with the same shape,
RMAIL_RULES__TICK_INTERVAL or RMAIL_RULES__DRY_RUN_DAYS. Nothing prints the
effective values back at you, since there is no rule verb and no config verb,
so the file and the environment are the whole answer — and which rules exist
is a ListRules call, not a line in either.

## Where to read next

- [[practice-rules]] — the one-sentence version of this page.
- [[saved-vs-smart]] — when a smart folder is the better answer than a rule.
