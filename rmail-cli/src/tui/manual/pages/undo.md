# Undo, and what cannot be undone

{{keys:outbox.cancel}} does two different things depending on what it finds,
checked in this order: cancel a send still inside its window, or — if there is
none *and the outbox pane is not what is currently on screen* — pop the last
reversible action (a move, a flag, a tag) off a session-local stack and
reverse it. Inside the outbox pane itself the key stays scoped to sends only,
even with nothing there yet to cancel — its own status line already promises
"u cancels the highlighted send", and answering a different question than the
one on screen would be worse than saying there is nothing to do. The window
exists because a send is time-critical in a way nothing else here is: past
it, mail has actually left, and there is nothing left to catch up with. See
"The stack" below for the second half.

## Every send is scheduled

An immediate send is really a send scheduled for now plus the undo window,
which defaults to ten seconds. That is what makes undo a cancel rather than a
recall: nothing has been transmitted, so there is nothing to catch up with.
Setting the window to zero makes sends truly immediate and removes the
countdown along with it.

After an immediate send the status line carries a countdown toast, and
{{keys:outbox.cancel}} takes it back — {{cmd:outbox cancel}} does the same
thing by id. From the message list that key means the send the toast is
offering, which is the only one visible from there — and when several windows
are open at once, the countdown is the earliest of them, the one about to
stop being undoable.

Only a window of two minutes or less gets a toast at all, and that ceiling is
compiled in rather than configured. Past it the row is a schedule rather than
an oops window, and a countdown would hold a line of the screen and repaint
the whole view once a second until it expired; the outbox pane shows those,
with no countdown.

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
It becomes failed only when its retries are exhausted — five attempts by
default, the delay starting at thirty seconds and doubling to a thirty-minute
ceiling — never merely because the machine was offline. A send that came due
while the daemon was down goes out when the daemon returns; more than ten
minutes overdue it is flagged as sent late, which is a note on the row and not
a refusal. Nothing is ever dropped for being late.

## Sends an agent asks for

A send originating from MCP always gets an undo window, so a human can
intercept it. Turning off the confirmation requirement shortens that window to
a hard floor of ten seconds; it cannot remove it. With the requirement on the
floor follows send.undo_window instead, so an agent can never ask for less
than one of your own sends gets; with it off the floor is ten seconds flat. At
the default ten-second window those are the same number, so turning the
requirement off changes nothing until you lengthen the window. A request
asking for a zero window, or for a send at exactly now, is the same bypass in
different clothes: an agent's send is pushed back out to the floor either
way.

## The stack

A move (including an archive), a flag toggle, or a tag *addition* usually
pushes an entry onto a session-local stack, capped at 200 — past that the
oldest is dropped to make room, not the newest. Outside the outbox pane,
{{keys:outbox.cancel}} pops the most recent one and reverses it once there is
no send left to cancel first — including a send the toast is still offering.
From *inside* the outbox pane the key stays scoped to sends only and never
reaches the stack — see the opening section above — but that scope still
favors a live toast over an empty or broken listing: a highlighted row wins
when there is one, otherwise the toast does, and only with neither does the
key say there is nothing to cancel. That is what keeps a slow or failed
outbox listing from making an armed countdown uncancellable just because
that pane happens to be up. A bulk action on several messages at once pushes
one entry per message it actually changes, not per message touched — a
mixed selection where some rows already match the target pushes fewer
entries than rows selected, so undoing all of it can take fewer presses than
the selection's size. There is no single "undo the whole batch" entry.

**No redo.** Undoing an undo does not put the original action back — a popped
entry is gone, reversed, done. This is deliberate: replaying a forward action
against a mailbox that has since changed underneath it (a message moved again,
deleted, its flags touched from another client) would be a guess dressed up as
a fact. The same caution means a reversal that itself fails is not retried
automatically or put back on the stack — the status line says so plainly
rather than claiming it happened, but the specific entry is gone either way.

Switching accounts clears the stack — an entry left over from the account you
just left has nowhere on screen to show what undoing it did.

## What cannot be taken back

- Delete, which expunges on the server. See [[archive]].
- A move whose source folder this client cannot vouch for — a message opened
  from a search result or a similar cross-folder view, rather than from the
  list of the folder it is actually in. Moving it still works; there is
  simply nothing honest to record as "where it came from" to undo back to.
- Removing a tag. The daemon's own answer to "remove this tag" carries no way
  to tell "it was there and now is not" apart from "it was never there" —
  guessing the former and undoing wrong would *add* a tag the message never
  carried, silently, which is worse than not offering the undo at all. Adding
  a tag has no such gap and is undoable normally.
- A message whose window has closed. The toast disappears when it does,
  because an undo offer that no longer works is worse than no offer.
- Anything that already left the machine: a webhook delivery, a model call,
  an IMAP keyword that has already round-tripped.

## Where these numbers come from

All of them live in the [[config-file]]'s send table, and a fresh install runs
on these:

```toml
[send]
undo_window = "10s"
max_retries = 5
backoff_base = "30s"
backoff_max = "30m"
poll_interval = "30s"
late_tolerance = "10m"
ai_requires_confirmation = true
workers = 2
```

Two of those read as slower than they are. poll_interval is a ceiling on the
scheduler's sleep and not a tick, so a send due in two seconds goes out in two
seconds; the interval only bounds how late a suspended laptop's send can be,
which is also what late_tolerance exists to tell you about. And workers is
deliberately small — a backlog drained fifty-at-once looks exactly like an
outbound spam run to the receiving server.

Every field takes an environment override of the same shape,
RMAIL_SEND__UNDO_WINDOW or RMAIL_SEND__MAX_RETRIES, so trying a longer window
for an afternoon needs no edit. For one message rather than all of them,
mail send --undo-window takes a number of seconds.

The window in force right now is never a guess: an immediate send prints the
id to undo and the seconds left to do it in, and {{cmd:outbox}} lists every
row with its deadline and, for a failed one, its last error.

## Where to read next

- [[practice-sending]] — how to use the window rather than fear it.
- [[practice-followups]] — the other thing to do at send time.
