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

## Secrets are referenced, never inlined

A signing secret is resolved from the keychain, an environment variable, or a
command — the same way an account password is. A secret in a config file is a
secret in a backup.

## And check deliveries before you debug anything else

{{capability:WebhookListDeliveries}} distinguishes "never generated" from
"generated and not delivered", which is most of the diagnosis.
{{capability:WebhookReplayDelivery}} is the only way out of a failed delivery
once its attempts are spent. [[digest-to-slack]] walks the whole path.
