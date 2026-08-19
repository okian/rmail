# Undo, and what cannot be undone

There is no general undo stack. What exists is narrower and more reliable: a
window during which a send has not happened yet.

## Every send is scheduled

An immediate send is really a send scheduled for now plus the undo window,
which defaults to ten seconds. That is what makes undo a cancel rather than a
recall: nothing has been transmitted, so there is nothing to catch up with.
Setting the window to zero makes sends truly immediate and removes the
countdown along with it.

After an immediate send the status line carries a countdown toast, and
{{keys:outbox.cancel}} takes it back — {{cmd:outbox cancel}} does the same
thing by id. From the message list that key means the send the toast is
offering, which is the only one visible from there.

## The outbox

{{keys:outbox}} lists everything scheduled, failed, or still inside its
window. {{cmd:outbox}} is the same list from the command line. A row can be
rescheduled, edited, retried, sent now, or cancelled:

```
mail outbox                 everything pending
mail outbox show <id>       one row, with its last error
mail outbox cancel <id>     what the toast key does, by id
mail outbox reschedule <id> --at "friday 5pm"
mail outbox retry <id>      a failed row, again
```

A send that fails transiently goes back to scheduled with a next attempt time.
It becomes failed only when its retries are exhausted — never merely because
the machine was offline.

## Sends an agent asks for

A send originating from MCP always gets an undo window, so a human can
intercept it. Turning off the confirmation requirement shortens that window to
a hard floor; it cannot remove it.

## What cannot be taken back

- Delete, which expunges on the server. See [[archive]].
- A message whose window has closed. The toast disappears when it does,
  because an undo offer that no longer works is worse than no offer.
- Anything that already left the machine: a webhook delivery, a model call,
  an IMAP keyword that has already round-tripped.

Reversible actions — a move, a flag, a tag — are undone by doing the opposite,
which is not an undo stack but is available forever rather than for ten
seconds.

## Where to read next

- [[practice-sending]] — how to use the window rather than fear it.
- [[practice-followups]] — the other thing to do at send time.
