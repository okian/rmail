# Practice: let the undo window do the checking

Send it. Do not re-read it before pressing send, and do not set the undo
window to zero.

## Why

Ten seconds of countdown catches the wrong recipient and the missing
attachment far more reliably than one more read-through does, because you
only look properly once it has gone.

## What is actually happening

An immediate send is a send scheduled for now plus the window, so undo is a
cancel and not a recall. Nothing has been transmitted. The window is ten
seconds on a fresh install, and nothing about this habit asks you to change
that. See [[undo]].

## Reply and forward create drafts

{{keys:message.reply}} and {{keys:message.forward}} create a draft; this
client never assembles a message itself. {{cmd:message reply}} is the same
call by name. A draft is edited, then sent — and the send is what carries the
window, which is why nothing here is a one-keystroke path from reading a
message to it having left the building.

## Let the guardian look too

The pre-send check reads for the things people actually get wrong: "see
attached" with nothing attached, an unfilled placeholder, an apparent secret,
a recipient who does not belong. Its deterministic findings can refuse a
send; a model finding never can, which is why turning the model half off
changes nothing about what this daemon refuses.

Both halves are on out of the box — send.preflight.enabled and
send.preflight.ai are true — and send.preflight.block_at is block, so only
an apparent secret and an unfilled placeholder refuse a send outright. A
missing attachment, or more than the fifteen recipients
send.preflight.max_recipients allows, is a warning — and the automatic check
does not show you its warnings, it lets the send go. The review you get to
read is the one you ask for, {{capability:SendSchedulerPreflightCheck}}, and
it answers even when the automatic check is switched off. mail send --force
is the documented way past a refusal once you have read it, and every use of
it is logged.

A daemon with ai.enabled false has no guardian at all — the deterministic
half does not run on its own — and neither does one whose AI provider could
not be built. On those machines the window is the only pre-send check there
is, which is the strongest argument for leaving it alone.

## And for anything an agent sends

An MCP-originated send always carries a window so a human can intercept.
Shortening it is possible; removing it is not. The floor is ten seconds, and
send.ai_requires_confirmation, true out of the box, is what makes an agent's
send wait the longer of that floor and send.undo_window — turning it off
drops the wait back to the floor rather than removing it. The default window
is that same ten seconds, so the setting changes nothing until you have
lengthened send.undo_window past it.

## Where the window is set

send.undo_window, in the [[config-file]]'s send table, ten seconds out of the
box. RMAIL_SEND__UNDO_WINDOW changes it for one run of the daemon without an
edit, and mail send --undo-window lengthens it for a single message.

Lengthening it has a ceiling worth knowing about: only a window of two
minutes or less gets a countdown toast, and that number is compiled in rather
than configured. Past it the send is still cancellable, from the outbox
rather than from a toast — {{keys:outbox}} lists it, and
{{cmd:outbox cancel}} takes it back by id. Which is the argument for keeping
the window in seconds: the countdown in front of you is the whole mechanism,
and a window you have stopped noticing is not checking anything.
