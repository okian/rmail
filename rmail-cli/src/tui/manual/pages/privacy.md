# Privacy and what leaves the machine

Everything is local until something explicitly sends it somewhere. This page
is the list of things that can, and what stands between them and your mail.

## The four ways mail can leave

- A model provider, when an AI feature runs. Governed by the policy engine,
  the redaction firewall and the budget.
- Your mail server, which already has your mail — a move, a flag or a
  keyword going back is not egress, but a tag round-tripping as an IMAP
  keyword does put a label of yours on their server.
- A webhook destination you registered. Off by default, and the only
  surface in rmail that puts mail content on a third party's server. See
  [[practice-webhooks]].
- Your browser. {{keys:message.open-html}} hands the HTML alternative to it,
  and from that moment the page's remote images and trackers are the
  browser's business rather than rmail's. That is the reason it is a separate
  key and not what {{keys:open}} does.

## The redaction firewall

Before any hosted model call, emails, phone numbers, card numbers, postal
addresses, secrets and names are reversibly tokenized in memory, and the
response is re-hydrated on the way back. The provider sees placeholders. It
is a pre-flight step on the call path rather than an option on each feature,
so a new AI feature is redacted by construction.

It runs out of the box: ai.privacy.redact is true. Four of the nine kinds it
knows — email, phone, postal address and name — are on whenever redact is,
and are not listed anywhere you could narrow them. The other five track
ai.privacy.redact_patterns by name, and a fresh install names all five:
ssn, credit_card, iban, api_key, otp. Removing a name shrinks what the pass
looks for; setting redact to false leaves the stage in the call path and
gives it nothing to find, which is why that is the audited opt-out rather
than a bypass.

Two more fields in the same table bound the payload before the firewall ever
sees it. strip_attachments is true, so the text extracted from attachments
is never appended to the body a queued AI job carries; max_body_chars is
40000, the length at which that body is cut. All of it lives in the
[[config-file]]'s ai.privacy table, and each field takes an environment
override of the usual shape, RMAIL_AI__PRIVACY__REDACT or
RMAIL_AI__PRIVACY__MAX_BODY_CHARS, so a one-off does not need an edit.

## Policy, not preference

An AI policy rule declares, per account, per folder or per pattern, one of
three states: allowed, local-only, or forbidden. A forbidden folder is
invisible to every AI path — not filtered out afterwards, not summarized and
withheld. Every decision is logged and can be explained.

Local-only forces the on-device path, which is a different provider rather
than a stricter prompt: nothing about the call reaches the network.

A fresh install has no rules at all: ai.policy.default_mode is allowed and
ai.policy.default_residency is unspecified, so mail no rule covers is
eligible, tagged with a residency a caller must read as unknown rather than
as compliant. Allowed is deliberate. The redaction firewall already stands
between every allowed resolution and an outbound call, so defaulting to
forbidden would duplicate a protection that exists one stage later and cost
you a pipeline that does nothing until every account is allow-listed. The
property being defended is that an explicit boundary is never silently
crossed — a rule always beats the default, and two rules that disagree
resolve to the more restrictive of the two.

Two switches sit above every rule, and nothing below can override either.
ai.enabled, true by default, is the global kill switch: false is forbidden
for every account and every folder, and no rule beneath it is reached at
all. The ai.enabled under an individual accounts entry is also true by
default, and setting that one false is a hard opt-out for one account — a
folder or pattern rule cannot reopen an account you shut off there, which is
what makes it the switch to reach for when the answer is "not this mailbox,
ever".

## Prompt injection

Mail is attacker-controlled text. Bodies are wrapped in labelled
untrusted-content delimiters and declared to be data rather than instructions
on every path that shows one to a model — unconditionally, with no switch to
turn that part off. On top of that, {{capability:AiSafetyScanInjection}}
looks for instruction overrides, forged tool framing, exfiltration requests,
zero-width characters and hidden text, and a flagged message withholds any
AI-decided action until a human confirms it.

That detector is on out of the box — ai.injection.enabled is true — and the
severity at which it withholds an action is ai.injection.block_actions_at,
hostile by default: text addressed to the model, meaning an instruction
override, forged system or tool framing, an exfiltration request.
Obfuscation on its own — zero-width characters, homoglyphs, CSS-hidden text
— is still scanned for and still recorded, but it is classed suspicious, one
step below the gate, because enough ordinary marketing mail carries it that
gating on it would teach you to turn the gate off. Set block_actions_at to
suspicious to include it; never is the third value. Neither knob reaches the
structural half above, which has no setting at all.

## The ledger

Every model call is recorded append-only: model, tokens, cost, redaction
level, latency, and a SHA-256 of the exact payload sent. Every AI artifact
links back to its entry, so "what did this summary actually cost, and what
was sent to produce it" has an answer rather than an estimate.
{{capability:AuditQueryAiCalls}} reads it; {{capability:AuditExportLedger}}
takes it out.

The redaction level on an entry is that one call's own answer, and it is the
field to read when you want the behavior rather than the configuration:
redacted means the pass replaced something in that payload, none means it
found nothing to replace there — not that it was skipped.

## What a fresh install sends, and what it does not

None of this has to be written down for it to be what you are running:

```toml
[ai]
enabled = true
provider = "claude"
api_key_command = "security find-generic-password -s anthropic -w"

[ai.privacy]
redact = true
redact_patterns = ["ssn", "credit_card", "iban", "api_key", "otp"]
strip_attachments = true
max_body_chars = 40000

[ai.policy]
default_mode = "allowed"
default_residency = "unspecified"

[ai.injection]
enabled = true
block_actions_at = "hostile"

[index.semantic]
provider = "local"

[webhooks]
enabled = false
```

AI is on, and the key is deliberately not in that file: the default
api_key_command asks the macOS Keychain for an item named anthropic, and its
stdout is read at the moment of a call. So a host nobody has given a key to
makes no hosted call — but enabled = true is not inert, because per-message
triage at sync time starts spending the moment a key resolves.
Search is the same shape from the other side: index.semantic.provider is
local, so embedding your mail for search runs on this machine, and the local
embedder will not even fetch its own weights until
index.semantic.local.allow_download is turned on. Webhooks need two switches
thrown — webhooks.enabled, and a destination existing — and ship with
neither.

Account discovery is the one thing that reaches the network before an
account exists. mail account add is given the domain and never the local
part, and it asks four places:

```
https://autoconfig.<domain>/mail/config-v1.1.xml
https://autoconfig.thunderbird.net/v1.1/<domain>
https://autodiscover.<domain>/autodiscover/autodiscover.xml
https://dns.google/resolve             SRV and MX, over DNS-over-HTTPS
```

The two that are not derived from your own domain — Mozilla's ISPDB, which
Thunderbird ships against, and the DNS-over-HTTPS resolver — are compiled
in rather than configured; no key in rmail.toml and no environment variable
repoints them. Nothing is written by any of it either: the command prints a
TOML block for you to paste, so a discovery cannot alter an account you
already have. The --ai flag is the exception worth knowing, and it is off
unless you type it — on a miss from all four probes it hands the domain, its
MX hosts and the probe response bodies to Claude, which is shown no mail
content and, because the local part is dropped before that evidence is
built, is never told whose mailbox this is. See [[add-any-account]].

## Where to read next

- [[ai-cost]] — the budget side of the same machinery.
- [[practice-accounts]] — the coarsest and most reliable control of all.
