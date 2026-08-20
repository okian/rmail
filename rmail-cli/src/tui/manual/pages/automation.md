# Webhooks, hooks and notifications

Three ways rmail acts on its own when mail arrives, and they are worth telling
apart by where they send things. A **hook** runs a command on this machine. A
**notification** pings this machine's desktop. A **webhook** POSTs your mail to
somebody else's server — it is the only outbound-network surface for mail content
in the whole client, and everything about how it is spelled follows from that.

## Webhooks: what leaves the machine

{{cmd:webhook list}} is every registered destination, with what it subscribes to,
whether it is entitled to message bodies, and where its signing key comes from —
never the key itself.

The URL column shows only `scheme://host` unless you ask. That is not tidiness: a
webhook URL is frequently the credential. Anyone holding
`https://hooks.slack.com/services/T…/B…/…` can post to that channel, so the
routine listing does not put it on screen (or into a model's context, for the
same tool projected over MCP). `--reveal-url` shows it in full.

A destination entitled to bodies is drawn as a warning. The default payload is a
notification — sender, subject, message id, a deep link, the account and mailbox,
and nothing else. Not the body, not attachments, not recipients, not headers.
`--include-body` opts one destination in to the body text itself, redacted through
the same firewall that governs text sent to a model, on every matching message.
That is a property of the destination you registered and never of a request: a
caller cannot ask for more of a message than the destination was configured to
receive.

{{cmd:webhook add}} registers one:

```
mail webhook add https://hooks.slack.com/services/T00/B00/xxx --name eng-alerts --template slack --events on_new_message,on_rule_match
```

`--template slack` renders Slack's incoming-webhook shape with `&`, `<` and `>`
escaped, so a hostile subject cannot render as a link that lies about where it
points. `--events` takes a comma-separated list or repeats; omit it entirely and
the destination receives only an explicit {{cmd:forward}} and no firehose.

The signing key is a *reference*, exactly as an account password is:
`--secret-env`, `--secret-command` or `--secret-keychain`, one of them, never the
key. Without one the payload is unsigned, and the listing says `unsigned` rather
than implying a receiver can verify anything.

{{cmd:webhook disable}} stops sending without forgetting where a destination
pointed. Queued deliveries stay queued and stop being claimed, so
{{cmd:webhook enable}} resumes rather than replays. {{cmd:webhook rm}} forgets the
destination **and its delivery history** — the record of what already left this
machine — which is why it is one of the few verbs here that asks first, and why
its question names `disable` as the reversible answer.

## The delivery queue

Nothing is POSTed inline. Every delivery is a durable row: one per
(destination, event), so a redelivered sync or an overlapping dispatch tick cannot
produce two POSTs, and each row carries its own frozen body, its attempt count and
its cap.

{{cmd:webhook deliveries}} is that queue, newest first. `--destination` narrows to
one, `--limit` bounds it, and `--show-payload` includes the exact bytes — off by
default, because a queue listing is frequently pasted into a ticket and the
payload is the mail content the rest of the view deliberately does not restate.

Failures back off exponentially, and a row whose attempts are spent goes to
`failed` and stops. {{cmd:webhook replay}} is the only way back out, and Enter on
a failed row runs it. It asks first: replaying POSTs the same content to a third
party again. It resends the *frozen* bytes under the same delivery id, not a fresh
render of a mailbox that has since changed.

{{cmd:forward}} queues one message to one destination now:

```
mail forward 42 --to eng-alerts
```

Note what the status line says: **queued**, never sent. The dispatcher sends it on
its next tick. If no dispatcher is running at all — `webhooks.enabled = false` —
the line says so outright, because the delivery is durably queued and will go out
whenever the dispatcher is switched on, which is not the same thing as having been
delivered.

## Hooks: what runs here

{{cmd:hook list}} is every configured hook, enabled and disabled alike, with the
event it fires on, its timeout and the command it runs.

{{cmd:hook test}} runs one immediately and reports exactly what happened —
`exit 0`, a non-zero code, killed for exceeding its timeout, cancelled by a daemon
shutdown, or never spawned at all. Those are five different facts and the report
keeps them apart. `--event-json` pipes your own payload to the hook's stdin
instead of a synthetic sample; the daemon checks it parses as JSON and never
interpolates it into the command, which is the one invariant the whole hook surface
exists to protect.

{{cmd:hook add}} does **not** write anything. Hooks live in the config file's
own array of hook tables and `HookService` has no Create, deliberately: a setting
that lives in your config file must not also live in a database the daemon then has
to keep in sync with it. So this renders the exact block, names the file, and says
when it takes effect:

```
[[hooks.hooks]]
name = "notify"
event = "on_new_message"
command = "/usr/local/bin/notify-me"
timeout = "10s"
```

`mail hook add on_new_message --name notify -- /usr/local/bin/notify-me` *does*
append that block for you, because a one-shot command can read, append, validate
and exit. A TUI holding the same file open across a session
cannot: it has no idea what else has edited the file since it started, and the
daemon it is talking to has already loaded its own copy. {{cmd:toml}} opens the
rendered block so it can be copied.

## Notifications: what interrupts you

Every newly synced message is scored into a tier — `low`, `normal`, `high`,
`critical`, the same ladder the triage pass uses — with a one-line reason, and a
desktop notification fires only at or above the account's threshold.

{{cmd:notify list}} is the live feed. It has no end: it keeps listening until you
close it, and `--since <id>` replays everything after that alert first. Omit it and
you get only what fires from now on, which is what a terminal wants.

{{cmd:notify score}} answers for one message, and the interesting answer is usually
not the tier. `queued` means it has not been scored yet and this call has asked for
it — scoring goes through the shared AI queue, with its policy, redaction, budget
and audit gates, so nothing here blocks on a model call. When a message was scored
and you were not told, the rows that explain it are `threshold` (what it was
measured against), `account` (whether notifications are on for that account at
all) and `suppressed`.

{{cmd:notify set}} renders the notify table, for the same reason
{{cmd:hook add}} does — there is no SetThreshold RPC, deliberately:

```
[notify]
threshold = "high"
include_subject = true
```

A threshold outside the four tiers delivers *nothing* and only warns at daemon
startup, so a typo there is notifications silently switched off. This refuses one
rather than rendering it. To switch notifications off, set `enabled = false`
(`--disabled`) rather than inventing a tier for it.

## Where to read next

- [[digest-to-slack]] — the whole webhook path as a worked example.
- [[config-file]] — where hooks and thresholds live, and why.
- [[privacy]] — what leaves the machine, and the switches that decide.
- [[practice-webhooks]] and [[practice-notifications]] — one habit each.
