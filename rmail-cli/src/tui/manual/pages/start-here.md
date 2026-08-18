# rmail

A local-first mail client. Your mail lives in a database on this machine,
every screen reads it over gRPC from the daemon, and nothing here talks to
your mail server directly — which is why the same "archive this message" runs
whether you press a key, type a command, or ask an AI agent over MCP.

This manual is compiled into the binary. It needs no network, no daemon and
no files on disk, so it still works on the day the daemon will not start.

## Where to go

- [[manual]]
- [[keys]]
- [[commands]]
- [[modes]]
- [[capabilities]]

## The shape of the screen

Folders on the left, the message list in the middle, a preview on the right,
a status line underneath. Opening a message ({{keys:open}}) replaces all
three with a full-width viewer, because a third of an 80-column terminal is
not enough to read mail in. {{keys:back}} comes back out.

## Moving around

- {{keys:cursor.down}} and {{keys:cursor.up}} move the cursor. A count works
  the way it does in vim: 5j goes down five rows.
- {{keys:cursor.top}} and {{keys:cursor.bottom}} go to the ends — or, with a
  count, to that row.
- {{keys:focus.toggle}} switches between the folder pane and the message
  list; {{keys:focus.folders}} and {{keys:focus.messages}} go straight to one.
- {{keys:quit}} leaves from anywhere and cannot be rebound. Neither can Esc,
  which always backs out of whatever is innermost.

## Acting on mail

Every one of these acts on the visual selection when there is one
({{keys:visual.toggle}} starts it), on the message in the viewer when it is
open, and otherwise on the row under the cursor.

- {{keys:message.archive}} moves the message to the account's archive folder.
  There is no Archive RPC: archiving is a move, and which folder counts as
  the archive is a per-server convention rather than an operation.
- {{keys:message.toggle-read}} and {{keys:message.toggle-flag}} set flags.
  Over a mixed selection they pick one intent for the whole selection rather
  than toggling each row, so marking a half-read selection read does not
  leave the read half unread.
- {{keys:message.copy}} and {{keys:message.move}} open a folder picker.
- {{keys:message.delete}} asks first, because {{cmd:message delete}}
  expunges on the server: the message leaves the account, it does not go to a
  trash folder.
- {{keys:message.reply}} and {{keys:message.forward}} create a draft. This
  client never assembles a message itself.
- {{keys:message.open-html}} hands the HTML alternative to your browser.

## Finding things

- {{keys:search}} is ranked search over your mail. A leading ~ makes it
  semantic, a leading = makes it lexical, and Tab completes an operator
  such as from: or has:attachment.
- {{keys:search.explain}} on a result says why it matched.
- {{keys:finder}} is the fuzzy jump-to-anything: > scopes it to commands,
  # to tags, @ to people, / to saved searches, : to folders.
- {{keys:palette}} runs a command by name.
- {{keys:ask}} asks a question about the mailbox and streams an answer with
  citations. The daemon decides whether that answer was grounded in mail it
  actually retrieved; the model never gets to vouch for itself.

## The AI panel, and what it costs

{{keys:ai.panel}} shows the cached analysis for whatever the cursor is on —
already paid for, by the triage pass that ran at sync time. Nothing in that
panel spends anything.

{{keys:ai.quick}} is the menu that does: a fresh summary, a question, or a
suggested reply are model calls, and they are behind their own key for that
reason. {{cmd:ai.quick}} pins the panel to the message you aimed it at, so a
folder reloading underneath you cannot throw away an answer you paid for.

## Sending, and taking it back

{{keys:outbox}} lists scheduled, failed and still-undoable sends.
{{keys:outbox.cancel}} cancels the one under the cursor — or, from the
message list, the one the countdown toast is offering, which is the only send
you can see from there.
