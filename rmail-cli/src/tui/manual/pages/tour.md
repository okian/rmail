# A tour of the screen

Three panes and a status line. Folders on the left, the message list in the
middle, a preview of whatever the cursor is on to the right. Everything the
list shows came from the local database, so it is drawn before the daemon has
finished saying hello.

## Moving the cursor

- {{keys:cursor.down}} and {{keys:cursor.up}} move one row. A count works
  the way it does in vim: 5j goes down five rows, and a count that runs off
  the end stops at the end rather than wrapping.
- {{keys:cursor.top}} and {{keys:cursor.bottom}} go to the first and last
  rows — or, with a count, to that row, so 12G is the twelfth message.
- {{keys:focus.toggle}} moves between the folder pane and the message list.
  {{keys:focus.folders}} and {{keys:focus.messages}} go straight to one, which
  is what you want when you already know where you are going.

## Opening and leaving

- {{keys:open}} opens whatever the cursor is on: a folder loads its messages,
  a message opens the full-width viewer. A third of an 80-column terminal is
  not enough to read mail in, so the viewer replaces all three panes rather
  than growing the preview.
- {{keys:back}} leaves the viewer. From the message list it quits, because
  there is nothing further back to go to.
- {{keys:cancel}} are the chords that back out of something. Esc is the one
  that means it in every layer — an overlay, then a selection, then the
  viewer — and it cannot be rebound, nor can {{keys:quit}}, because a config
  file that could take away the way out could lock somebody into a modal
  screen. The rest are local to one layer: q closes a menu, n declines a
  confirmation, ? closes the help it opened.

## Reaching everything else

{{keys:palette}} runs any command by name, which is the answer to "there must
be a key for this and I do not know it". It ranks what you type against every
command's name and description, so typing a word from the description finds
the verb even when you cannot remember its spelling.

The three other doors out of the message list are [[search-vs-finder]] for
finding mail, [[bulk]] for acting on more than one message, and this manual —
{{keys:manual}} from anywhere, described in [[manual]].

## Where to read next

- [[daemon]] — what is running behind the screen, and what happens when it
  is not.
- [[typing]] — the keys that only exist while something is asking you a
  question.
- [[archive]] — what the four message-moving keys actually do to your
  server.
