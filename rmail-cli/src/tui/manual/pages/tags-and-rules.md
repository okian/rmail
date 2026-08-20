# Tags and rules

Two ways to put mail into categories, and they are not the same thing. A tag is a
label you or the model attaches to a message. A rule is a standing instruction
that acts on mail as it arrives — archiving, tagging, forwarding — and the rules
here are `RuleService`'s only surface anywhere, so these spellings are what a
future `mail rule` will have to adopt.

## Tags you apply yourself

{{cmd:tag add}} and {{cmd:tag rm}} act on the message under the cursor, or on
every message in a visual selection if one is up — `:'<,'>tag add invoices` is
the same set the keys act on, which is the whole reason the range spelling
exists. The report shows a row per message, because a tag that applied to four of
five and failed on the fifth is the outcome worth seeing and a count hides it.

{{cmd:tag list}} is every tag with how many messages carry it. A tag nothing
carries is drawn dim rather than hidden, so a tag you just created does not look
as though nothing happened. {{cmd:tag new}} creates one; `--color` sets a colour
and `--sync` decides whether it lives only here (`local`), as an IMAP keyword
(`imap`), or whatever the server supports (`auto`).

{{cmd:tag bulk}} is the one that looks interchangeable with a ranged
`:tag add` and is not. It applies a tag to everything a *query* selects, in one
transaction, including mail this client has never loaded — so it takes the query
rather than a selection:

```
mail tag-bulk --query 'from:stripe is:unread' --account 1 invoices
```

## Tags the model suggests

{{cmd:tag suggest}} classifies the message you are on and streams what it finds:
a tag, how confident it is, and why. Enter accepts the highlighted suggestion and
{{keys:report.reject}} rejects it, both without leaving the list — the common
reply is "not that one", and a screen where rejecting meant typing would make the
safe answer the awkward one.

{{cmd:tag accept}} and {{cmd:tag reject}} do the same by id, which is what the
rows run underneath.

## Letting a suggestion apply itself

By default every suggestion waits for you. {{cmd:tag rules}} lists the rules that
change that, and {{cmd:tag rules set}} writes one:

```
mail tag-rules set newsletters newsletters --mode auto --min-conf 0.95
```

`--mode auto` is the one setting on that screen worth finding at a glance, and it
is drawn as a warning for that reason: above its confidence threshold, a model's
guess changes your mailbox with nobody looking. `suggest` is the default, and
without a rule at `auto` nothing is applied on your behalf. `--disabled` stores a rule
retired rather than deleting it.

## Rules

{{cmd:rule list}} is what exists. {{cmd:rule run}} evaluates the enabled rules
against the messages you have selected and applies nothing — a dry run, so you
can see what would have matched before anything does. `--rule <name>` narrows it
to one.

{{cmd:rule new}} is the one to reach for first. Say what you want in words and it
drafts a rule, then shows you its dry run over real mail:

```
mail api call RuleService.SynthesizeRule '{"account_id":1,"instruction":"archive newsletters","days":30}'
```

Two things in that report are worth reading before anything else. If the draft
asked for a criterion the daemon refused to include, the report says so under
`— dropped —`, and the rule that will actually run is *narrower* than what you
asked for. And `— summary —` is how much of your recent mail it matched: a rule
that matched everything is not the rule you wanted.

{{cmd:rule add}} stores that draft. It takes no argument on purpose: a rule is a
TOML document, and a one-line command cannot carry one — so the only thing it can
store is the draft whose dry run you have just read. A hand-authored file goes in
through `mail api call RuleService.CreateRule`, which is the same call.

{{cmd:rule backtest}} replays a stored rule over the last month and reports what
it would have done. `--days` changes the window. This is not the same as
{{cmd:rule run}}: a backtest looks at history, a dry run looks at what you have
selected now.

{{cmd:rule correct}} is how a rule that keeps getting one thing wrong is taught.
Quote the criterion and say which way it should have gone:

```
mail api call RuleService.RecordCorrection '{"account_id":1,"message_id":42,"prompt":"is a newsletter","expected":false}'
```

The reply says how many corrections that criterion now has, which is how you
know whether it has enough behind it to have changed.

## Where to read next

- [[reports]] — the screen these draw into, and what Enter does on a row.
- [[rule-from-mistake]] — the same ground as a worked example.
- [[practice-tags]] and [[practice-rules]] — one habit each, and why.
