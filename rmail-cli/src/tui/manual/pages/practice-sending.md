# Practice: let the undo window do the checking

Send it. Do not re-read it before pressing send, and do not set the undo
window to zero.

## Why

Ten seconds of countdown catches the wrong recipient and the missing
attachment far more reliably than one more read-through does, because you
only look properly once it has gone.

## What is actually happening

An immediate send is a send scheduled for now plus the window, so undo is a
cancel and not a recall. Nothing has been transmitted. See [[undo]].

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

## And for anything an agent sends

An MCP-originated send always carries a window so a human can intercept.
Shortening it is possible; removing it is not.
