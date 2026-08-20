# Reports

Most verbs answer with a sentence on the status line. Some answer with rows:
which folders the index has walked, which rules exist, what a run did. Those
open a report — one screen, the same one every time, whatever the verb was
about.

That is deliberate. A screen per subject would mean a different cursor, a
different Esc and a different confirmation in each of them, and you would
have to learn which was which before you could read any of them.

## Reading one

- {{keys:cursor.down}} and {{keys:cursor.up}} move a row, {{keys:cursor.top}}
  and {{keys:cursor.bottom}} jump to the ends. A count works here too.
- The columns are fixed at the widths the report declared. A streamed report
  fills in underneath the cursor rather than reflowing around it, so the row
  you are reading stays where it is.
- A row drawn with a return glyph has something behind it. Enter runs that.
  A row without one is information, and Enter on it does nothing.
- Each row carries a glyph as well as a colour, so a monochrome terminal
  still tells a healthy row from a failed one.

## Acting on a row

A row's action is a colon command — the same vocabulary you could have typed
yourself, so a row cannot do something the command line could not.

If that command changes anything, Enter asks first. Whether it changes
anything is read off the capability behind it, not off a list of dangerous
verbs kept in the client: {{capability:ClientAuthClearPassword}} removes a
password gate, so the row offering it asks, and a row that only reads does
not. Answering yes runs the command without asking a second time, which is
exactly what a trailing bang means on the command line.

Either answer leaves you on the report. Declining puts it back untouched;
running the command puts it back marked stale, because the rows above now
describe how things were rather than how they are. It is marked rather than
re-read on your behalf: the change is still in flight when the row's command
returns, so a refresh at that moment could redraw the state from *before* it —
a wrong answer with nothing saying so.

## Re-running and leaving

- {{keys:report.rerun}} runs the report's own line again, which is also what
  clears a stale marking. Rows from the previous run are cleared, the cursor
  stays where you left it, and a frame still arriving from the old run is
  discarded rather than mixed into the new answer.
- Esc closes the report and cancels whatever was still streaming into it.
  That matters for the long ones: a rebuild you stopped reading is work the
  daemon should stop doing, and the client saying so is the only way it can
  know.

## The one you have today

{{cmd:auth status}} reports the gate on rmail's own API, which is the report
worth knowing about before you need it: two rows from the daemon — whether a
password is set, and whether a local caller must log in as well — and one
from this client, naming which credential it is presenting. When something
answers UNAUTHENTICATED, those three lines together are the answer.

When a password is set, that row offers {{cmd:auth clear}} on Enter, and asks
first. Clearing it also forgets the session this client had cached for the
socket, because a cleared password makes that session moot — the same thing
`mail auth clear` does, for the same reason.

See [[daemon]] for what the gate is and how to set one up, and
[[practice-tokens]] for the other half of the same subject.

## What is capped here, and what is set elsewhere

A report keeps 500 rows, and 160 characters of any one cell. Neither is a
setting: the caps are there so a stream that misbehaves cannot grow this
client without bound while you read it. The daemon caps its own result sets
well below 500 rows, so that is not a number a report you asked for reaches;
the cell cap is, because one field of an answer can be as long as whatever
produced it. Past the 500th row an arriving frame takes only the room left,
or is truncated to fit, and the border counts the rows the pane actually
holds — that count, not the verb's own idea of how many there are, is what
you are reading. A cell cut at 160 characters ends in an ellipsis; each
column then fits its cell to the width the report declared, which is a
second cut at a different width and implies nothing about the first.

Neither verb here takes an argument or a flag, so there is nothing on the
line to default — {{cmd:auth status}} and {{cmd:auth clear}} are each the
whole command. The gate the report describes does have defaults, and they
live in the [[config-file]]'s client_auth table: there is no password until
mail auth setup sets one, require_for_local is false, so a socket peer is
trusted on its kernel-verified uid alone, and a session a login mints lasts
30 days. The environment overlay is the usual shape,
RMAIL_CLIENT_AUTH__REQUIRE_FOR_LOCAL. Which of those holds on this daemon
right now is a question for the report rather than for the file — the file
is where you change the gate, not where you check it.
