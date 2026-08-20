# Worked example: a digest into Slack

The goal: one message in a team channel each morning summarising what arrived,
without putting anybody's mail on a third party's server.

## First decide what actually leaves

A webhook is the only surface in rmail that sends mail content somewhere that
is not your mail server. It is off by default, and two switches have to be
thrown before anything goes out: the dispatcher, and a destination existing at
all.

The default payload is sender, subject, a deep link and — when the AI passes
have already run — a two-sentence summary. Never the body, unless that
destination was registered asking for it. Everything derived from message
content goes through the redaction firewall first. See [[privacy]].

## Register the destination

```
mail webhook add --name team-inbox --template slack \
  --secret-keychain rmail-webhooks https://hooks.example.com/services/x
```

The signing secret is referenced, not inlined: it is resolved from the
keychain, an environment variable or a command, exactly the way an account
password is. {{capability:WebhookRegister}} stores the destination as a live
row with its own delivery queue.

Everything that command does not say has a default: the payload template is
generic, the row is enabled the moment it exists — --disabled registers one
that is listed and never sent to — and the attempt cap on its deliveries is
five. The event subscription is the exception. A destination registered
without --events subscribes to nothing and receives only what you hand it by
name with mail forward; --events on_new_message is what makes it fire by
itself.

## Turn the dispatcher on

```
[webhooks]
enabled = true
```

That is the one line the dispatcher needs. false is the shipped value, and a
disabled dispatcher still lets you register, list and remove destinations —
what stops is the sending.

## Produce the digest

```
[digest]
enabled = true
interval = "24h"
model = "claude-sonnet-5"
```

Two of those three lines are the defaults written out: 24h is the shipped
interval and claude-sonnet-5 the shipped model, chosen because a briefing is a
synthesis task over many messages rather than the per-message classification
triage sizes for. enabled is the line that has to change. It is false out of
the box because a digest is a recurring Sonnet call, and something that spends
money on a timer has to be switched on deliberately rather than found on an
invoice.

Periods are absolute, anchored at the unix epoch, so 24h is a UTC day and not
a day counted from whenever the daemon last started — and one window gets
exactly one briefing, forever, unless you ask for another with mail digest
--force.

{{capability:AnalyticsGenerateDigest}} clusters what arrived, writes a
prioritised briefing with links back to the source messages, and does it on a
schedule. Preview one before you arm it:

```
mail digest --since 24h
```

With neither --since nor --until, that command briefs the last completed
period on the daemon's own cadence, which is the window the timer would have
covered.

## Check that it is being delivered

```
mail webhook deliveries --destination team-inbox
mail webhook replay <id>
```

A delivery is retried with backoff and stops at its attempt cap; a replay is
the only way back out of failed. Each attempt gets 15 seconds, the first retry
waits 30 seconds and every one after it doubles up to a 30-minute ceiling, and
the fifth failure moves the row to failed for good.
{{capability:WebhookListDeliveries}} is the log you should look at before
concluding that the digest did not run — the difference between "not
generated" and "not delivered" is most of the diagnosis. It reports the newest
20 rows and leaves out the JSON body that was POSTed unless you add
--show-payload, because a delivery listing is a thing people paste into
tickets.

## The two tables, and the one thing not in them

A fresh install runs the whole path on these, in the [[config-file]]:

```toml
[webhooks]
enabled = false
max_concurrency = 4
tick_interval = "5s"
delivery_timeout = "15s"
backoff_base = "30s"
backoff_max = "30m"
max_batch = 100

[digest]
enabled = false
model = "claude-sonnet-5"
interval = "24h"
tick_interval = "15m"
max_catchup_periods = 7
max_messages = 120
max_clusters = 15
max_context_tokens = 12000
max_chars_per_message = 800
max_tokens = 2048
```

Both tick_interval values are upper bounds on lateness and nothing else: five
seconds is how long a queued delivery may sit before the dispatcher drains it,
fifteen minutes is how late a briefing may be, and neither changes what the
briefing covers. max_catchup_periods is why a daemon that was off for a month
briefs the seven most recent periods and skips the rest instead of making
thirty model calls in one tick.

The sizing numbers are what one briefing is: at most 120 of the window's
messages, after clustering has ranked them, across at most 15 clusters, inside
12000 estimated tokens of assembled context — and at most 800 characters out
of any single message, so one enormous thread cannot spend the whole budget on
itself. That per-message figure is additionally clamped by the redaction
firewall's own body ceiling, which this path may not exceed merely because it
packs many messages at once.

Every field takes an environment override with the same shape,
RMAIL_WEBHOOKS__BACKOFF_MAX or RMAIL_DIGEST__MAX_MESSAGES, so a one-off does
not need an edit.

What is not in that file is the destination. It is a row in the daemon's
database, because it carries state TOML cannot hold — an event subscription, a
delivery history, an attempt cap of its own — so editing the config never adds
or removes one, and mail webhook list, not the file, is where you check what
exists.

## The one thing to get right

Send the link, not the mail. A digest that carries subjects and deep links
puts almost nothing on Slack's servers and is just as useful, because the
person reading it is one click from the real thing. See
[[practice-webhooks]].
