# Reply, drafts, sending and follow-ups

Everything {{keys:message.reply}} and {{keys:outbox}} reach from a key is
also reachable by name, plus a few things that have no key at all: an
AI-drafted reply, editing a scheduled send's body, chasing someone who has
gone quiet.

## Reply

{{cmd:reply}} is {{keys:message.reply}} by name: it creates a draft to the
sender of the message you are on, the same as pressing the key. Nothing
about that path spends anything.

{{cmd:reply}} `--ai` is different work. It streams a reply from an intent —
"push to Tuesday", or nothing at all for the shortest reply that moves the
thread forward — reading the thread and, when there is one, this account's
own past replies for voice. The words are shown as they arrive rather than
handed over all at once, and what gets stored either way is a draft: nothing
sends itself. `--reply-all` addresses everyone the parent addressed rather
than only its author, and needs `--ai`, since only that path resolves a
recipient list on its own.

## Drafts

Every draft this account holds is {{cmd:draft list}}. {{cmd:draft show}}
reads one back in full; {{cmd:draft edit}} replaces its body outright.
{{cmd:draft render}} builds the exact message a send would submit and
reports who it would actually reach, without sending it — the way to check
an address list before it matters. {{cmd:draft delete}} asks first, the same
reason {{cmd:index rebuild}} does: undone, not undoable.

{{cmd:draft rewrite}} asks the model to change a draft already written —
`--tone`, `--shorter` or `--longer`, or free-form `--instruction` text.
Every rewrite is kept: {{cmd:draft revisions}} lists them and
{{cmd:draft revert}} restores any one, `0` always meaning the words you
actually typed.

## Sending

{{cmd:send}} `--draft` schedules a stored draft — every message this build
sends starts as one, whether {{keys:message.reply}} created it or
{{cmd:draft rewrite}} rewrote it. `--at` names a time and `--undo` lengthens
the window past the account default. See [[practice-sending]] for what the
pre-send guardian actually reads for, and [[undo]] for what the window is a
cancel of.

A send the guardian blocks answers with its findings rather than a schedule
— {{cmd:preflight}} on the same draft shows the same list ahead of time, which
is the way to read what stopped it without having tried. This client has no
`--force` of its own: `mail send --force` on the same draft skips the
guardian from a shell, which is the only way past a block until a later task
gives this screen one too.

## The outbox

{{keys:outbox}} lists what is scheduled, sending, sent or failed.
{{cmd:outbox retry}} moves a failed one back to pending.
{{cmd:outbox reschedule}} and {{cmd:outbox edit}} change when a
still-waiting send goes out or what it says, without cancelling and
recreating it. {{cmd:outbox send-now}} skips the rest of the wait — it asks
first, since that is the one irreversible step in this list.
{{cmd:outbox suggest}} answers "when": the next moment inside the account's
configured business-hours guardrails, with a one-line reason — deterministic
in this build, not read from anyone's own history.

## Follow-ups, and who is waiting on you

{{cmd:followup new}} arms a reminder on the message under the cursor —
`--in` a delay, `--note` what to say when it fires. By default it cancels
itself the moment a reply arrives, so the common case needs no further
attention; that default lives in configuration, not in this verb.
{{cmd:followup list}} shows what is armed; {{cmd:followup dismiss}} clears
one by hand. {{cmd:waiting}} is the other direction: threads where the ball
is with somebody else, `--overdue` narrowing it to the ones already late.
{{cmd:nudge}} drafts a chase message for one of those — words only, nothing
sent — and {{cmd:preflight}} runs the same guardian {{cmd:send}} runs, ahead
of time, against a stored draft.

## Where to read next

- [[practice-sending]] — the undo window, and what the guardian actually
  checks.
- [[practice-followups]] — arming a reminder in the same breath as the send.
- [[undo]] — what a cancel actually is, and what nothing here can take back.
