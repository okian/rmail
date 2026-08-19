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

## Turn the dispatcher on

```
[webhooks]
enabled = true
```

## Produce the digest

```
[digest]
enabled = true
interval = "24h"
model = "claude-sonnet-5"
```

{{capability:AnalyticsGenerateDigest}} clusters what arrived, writes a
prioritised briefing with links back to the source messages, and does it on a
schedule. Preview one before you arm it:

```
mail digest --since 24h
```

## Check that it is being delivered

```
mail webhook deliveries --destination team-inbox
mail webhook replay <id>
```

A delivery is retried with backoff and stops at its attempt cap; a replay is
the only way back out of failed. {{capability:WebhookListDeliveries}} is the
log you should look at before concluding that the digest did not run — the
difference between "not generated" and "not delivered" is most of the
diagnosis.

## The one thing to get right

Send the link, not the mail. A digest that carries subjects and deep links
puts almost nothing on Slack's servers and is just as useful, because the
person reading it is one click from the real thing. See
[[practice-webhooks]].
