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

## Policy, not preference

An AI policy rule declares, per account, per folder or per pattern, one of
three states: allowed, local-only, or forbidden. A forbidden folder is
invisible to every AI path — not filtered out afterwards, not summarized and
withheld. Every decision is logged and can be explained.

Local-only forces the on-device path, which is a different provider rather
than a stricter prompt: nothing about the call reaches the network.

## Prompt injection

Mail is attacker-controlled text. Bodies are wrapped in labelled
untrusted-content delimiters and declared to be data rather than instructions
on every path that shows one to a model — unconditionally, with no switch to
turn that part off. On top of that, {{capability:AiSafetyScanInjection}}
looks for instruction overrides, forged tool framing, exfiltration requests,
zero-width characters and hidden text, and a flagged message withholds any
AI-decided action until a human confirms it.

## The ledger

Every model call is recorded append-only: model, tokens, cost, redaction
level, latency, and a SHA-256 of the exact payload sent. Every AI artifact
links back to its entry, so "what did this summary actually cost, and what
was sent to produce it" has an answer rather than an estimate.
{{capability:AuditQueryAiCalls}} reads it; {{capability:AuditExportLedger}}
takes it out.

## Where to read next

- [[ai-cost]] — the budget side of the same machinery.
- [[practice-accounts]] — the coarsest and most reliable control of all.
