# AI spend, safety and the ledger

Three questions with one answer each. What may a model call cost? Which backend
serves it? And what did the model actually do — including when a message tried to
give it instructions of its own.

## What has been spent, against what

{{cmd:ai budget status}} is spend so far today and this month, one row per
class, window and measure, each against the cap that measure is actually
compared with. Eight rows rather than four, because the row's colour is the
point: a scope over its soft token cap while still under its dollar cap is two
different answers, and a row that had to pick one tone for both would be telling
you the wrong one half the time.

The tones are the enforcer's own ladder. `✓` under every cap, `!` at or above a
*soft* cap — the model is being downgraded, opus to sonnet to haiku — and `✗` at
or above a *hard* one, where dispatch is blocked. A dimension with no cap at all
is dim and says `no cap`: unlimited is a configuration, not a warning.

The class label says where its caps came from. `all (set)` is an operator's
`SetBudget`; `all (ai.limits)` is the config file's default. They behave
identically right up until the configuration changes, so it matters which one you
are looking at before you edit it.

`--account 3` reports one account. Omitted, you get the *global* budget — the one
every call counts toward whichever account made it. Note that this is the
opposite of what a bare id means to most verbs here: `0` is a real scope for a
budget, not "no account", so nothing is inferred from whatever mailbox happens to
be open.

## Changing the caps

{{cmd:ai budget set}} opens a form, pre-filled with the caps in force:

```
mail ai budget status --account 1
```

The form exists because `SetBudget` **replaces** a scope's whole budget. A cap
the request leaves out is a cap *cleared*, so a line that set only the daily hard
cap would silently delete the monthly one. The form is filled in from the daemon
first, so applying it sends every cap in force rather than only the one you
touched — and it is visibly a set of values being replaced, which is what the RPC
does.

`j` and `k` move between fields, `<enter>` opens the highlighted one, `<enter>`
again keeps what you typed and `<esc>` puts back what was there. The last row is
`apply`; `<enter>` on it stores every field above it. An empty field is not
"leave this alone" — it is "no cap", and applying stores that. Clearing a cap is
how you remove it.

Flags on the line pre-fill the form rather than replacing it, so
`:ai budget set --daily-hard-usd=5` opens with that field holding `5` and the
rest holding whatever the daemon had. A trailing `!` skips the form entirely and
sends exactly what was typed, with the CLI's replace semantics — for a
keybinding, or for a line pulled back out of history by somebody who has already
decided:

```
mail ai budget set --account 1 --daily-hard-usd 5 --monthly-hard-usd 50
```

`--bulk` writes the sub-budget that only backlog work counts against. A bulk call
is checked against both, so exhausting it stops the backlog without touching what
interactive work may still spend.

## Which backend serves a call

{{cmd:ai provider status}} reports the config file's `ai.provider`, this scope's
own override, what the two combine to, and — for the on-device path — whether
this host could serve a call right now:

```
mail ai provider status 1
```

Two rows are worth knowing how to read. `network provider` says whether this
daemon holds a network-capable provider *at all*; under `ai.provider = "local"`
none is ever constructed, and the row is dim rather than red because that is the
local-only guarantee holding, not a fault. `local ready` is drawn red only when
local is the backend that will serve the next call — an unready local path on a
hosted install is a fact about the host, not a problem with it.

{{cmd:ai provider set}} takes `claude`, `local`, or `clear` to drop the override
and inherit again:

```
mail ai provider set 1 local
```

This can only narrow. `ai.policy` is resolved first, so `local` is always
honoured while `claude` is a *permission to use* the hosted backend where policy
already allows it, never a grant. A `local_only` folder stays on-device with a
`claude` override sitting right there.

## When a message tries to steer the model

Email is attacker-controlled text and this daemon feeds it to a model. The actual
control is structural — untrusted content is wrapped in explicit delimiters and
the model is told what is inside them is data, never instruction — and it needs
no command. What you can ask for is the detector's findings.

{{cmd:ai scan}} scans the message you are on exactly as the pipeline would see
it, including its raw HTML: hidden-text tricks are invisible by the time HTML has
been stripped to the plain text a prompt carries, so looking only at what the
model sees would miss the class of attack that hides itself from the human
instead. It makes no model call and costs nothing.

`severity` is `suspicious` for obfuscation with no legible instruction behind it
— zero-width characters, homoglyphs, CSS-hidden text, all common enough in real
marketing mail — and `hostile` when something in the text is addressed to the
model: an instruction override, forged system framing, an exfiltration request.
Below the severity rows is one row per detection, quoting what it tried.

The row that matters is `actions`. A rule whose `claude_is` predicate decided the
match will not fire on a message flagged at or above the configured threshold
until a human says so, and that row is where you say it: `<enter>` on it releases
the withheld actions, and `<enter>` on an already-confirmed message withdraws the
confirmation. It asks first, which is the one place in this client where a report
row asking `[y/N]` is exactly right — releasing a hold is consent to
AI-decided changes to your mail, and consent a machine can grant itself is not
consent.

{{cmd:ai confirm}} is the same thing by name, with `--revoke` for the other
direction. It rescans first either way, because a confirmation is consent to a
*specific* set of findings: the daemon clears it when a later scan turns up
different ones, so confirming without having just seen them would be consenting
to whatever a stale row happened to hold.

```
mail ai scan-injection 42 --confirm
```

## What the model actually did

{{cmd:ai audit}} is the append-only ledger: every request that went to a model
provider, what it cost, how long it took, what the redaction pass did to it, and
a hash proving what left the machine — not what the caller intended to send. It
cannot be edited or erased through this or any other surface.

Newest first, and not re-sorted here. `--model` narrows to one model, `--failed`
to the calls that failed, `--account` to one account; omit `--account` and you
get every one, which is what a ledger question usually means. `--all` walks the
whole ledger rather than the most recent page — still bounded by what a report
keeps, which the border says.

This is the ledger's first surface anywhere; there is no `mail ai audit` yet. A
script wanting the same rows goes through the API directly:

```
mail api call AuditService.QueryAiCalls '{"limit":50}'
```

## Where to read next

- [[ai-cost]] — the same spend from the pipeline's side, and what a downgrade
  actually does.
- [[privacy]] — what leaves the machine, and the config that decides.
- [[practice-budget]] — one habit, and why.
- [[reports]] — the screen these draw into, and what Enter does on a row.
