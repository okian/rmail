# Practice: send the link, not the mail

Register webhook destinations that carry sender, subject and a deep link, and
leave the body behind unless a specific destination genuinely needs it.

## Why

A notification exists to get somebody to open the message, and a payload that
contains the message is a copy of your mail on somebody else's server for no
extra benefit.

## What the default already does

The default payload is sender, subject, a deep link and — when the AI passes
have already run — a two-sentence summary. The body is included only for a
destination registered asking for it, and everything derived from message
content passes through the redaction firewall first. See [[privacy]].

## Two switches, both off

Webhooks are disabled by default and a destination has to exist. That is
deliberate: a hook's blast radius is this machine, a webhook's is the
internet.

The second switch is thrown by the registration itself — a destination is
enabled the moment it exists, and mail webhook add --disabled is how you
register one that is listed and sent nothing — not even an explicit
mail forward, which a disabled destination refuses — until you enable it.

## Secrets are referenced, never inlined

A signing secret is resolved from the keychain, an environment variable, or a
command — the same way an account password is. A secret in a config file is a
secret in a backup.

## And check deliveries before you debug anything else

{{capability:WebhookListDeliveries}} distinguishes "never generated" from
"generated and not delivered", which is most of the diagnosis, and prints each
row's attempts against its cap — five, unless the destination was registered
with another --max-attempts. {{capability:WebhookReplayDelivery}} is the only
way out of a failed delivery once those attempts are spent.
[[digest-to-slack]] walks the whole path.

## Where those numbers live

The retry schedule belongs to the dispatcher, in the webhooks table of the
[[config-file]]: delivery_timeout gives each attempt 15 seconds, backoff_base
makes the first retry wait 30 seconds, and every retry after that doubles the
wait until backoff_max stops it at 30 minutes. With the shipped cap of five
that ceiling is never reached — the four waits are 30 seconds, 1 minute, 2
minutes and 4 minutes, so a delivery is given up on about eight minutes after
its first attempt, and backoff_max starts mattering only on a destination
whose cap you raised. tick_interval, 5 seconds, bounds how long a queued
delivery sits before the dispatcher looks at it, and nothing else. Every field
takes an environment override of the same shape, RMAIL_WEBHOOKS__BACKOFF_MAX,
so a one-off does not need an edit.

The cap itself is not in that file, and neither is anything else a destination
is — its events, whether it was granted bodies, whether it is enabled. Those
are rows in the daemon's database, because they carry state a TOML file cannot
hold, so mail webhook list is where you read them back rather than the config.
Its table prints the events, the body decision and a disabled marker; the cap
is not in that table, only in mail webhook list --json.
