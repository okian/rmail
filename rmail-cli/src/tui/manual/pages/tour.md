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
- {{keys:cursor.page-down}} and {{keys:cursor.page-up}} move a screenful at a
  time, keeping one row of overlap so the line you were reading is still
  visible. Two spellings of each, the vim chord and the key with the name on
  it, doing the same thing. A count here means pages rather than rows — 3 then
  Ctrl-D is three screens, because 3j is already three rows. They are bound in
  every layer
  that has something to page: this list, the viewer, the manual, a result
  list and the folder picker. The layers without them have nothing to move
  through — a text field and a yes/no question — and the ? overlay, which
  shares the manual's layer but has no row cursor of its own, so they are
  inert there rather than absent. A page is measured against the terminal,
  so it changes when you resize the window, and until the first frame
  arrives the model assumes 24 rows.
- {{keys:focus.toggle}} moves between the folder pane and the message list.
  {{keys:focus.folders}} and {{keys:focus.messages}} go straight to one, which
  is what you want when you already know where you are going.

## Opening and leaving

- {{keys:open}} opens whatever the cursor is on: a folder loads its messages,
  a message opens the full-width viewer. The preview is 40 percent of the
  panes by default — 48 columns on a 120-column terminal, and nothing at all
  under 100, where it is dropped — so the viewer replaces all three panes
  rather than growing the preview.
- {{keys:back}} leaves the viewer. From the message list it quits, because
  there is nothing further back to go to.
- {{keys:cancel}} are the chords that back out of something. Esc is the one
  that means it in every layer — an overlay, then a selection, then the
  viewer — and it cannot be rebound, nor can {{keys:quit}}, because a config
  file that could take away the way out could lock somebody into a modal
  screen. The rest are local to one layer: q closes a menu, n declines a
  confirmation, ? closes the help it opened.

## Reaching everything else

{{keys:command}} opens the command line, which is the answer to "there must
be a key for this and I do not know it". Type a verb and press Enter; the
ranked list underneath matches on the verb's own name and on its description,
so a word from the description finds it even when you cannot remember the
spelling, and Enter runs the best match when what you typed is not a verb in
full. {{keys:palette}} opens the same line — the name is kept because
renaming an action would break a keys.toml somebody has already written.

Tab completes as far as the registry can be certain, and no further: two
verbs sharing a prefix stop at the prefix rather than one of them being
chosen for you. Up and Down walk what you have run before, filtered by
whatever is already on the line.

A verb takes the arguments it declares, and three things can precede or
follow it:

- {{cmd:helpgrep}} invoice — an argument, for the verbs that take one.
- '<,'> — the visual selection, so the verb acts on all of it. Opening the
  line while a selection is up fills this in for you. % and a leading count
  are part of the grammar and are refused here rather than half-honoured:
  nothing in this screen can address "every row listed" yet, and acting on
  one row instead would be a range that looked obeyed and was not.
- A trailing !, which skips the confirmation and nothing else — so
  {{cmd:message delete}}! expunges without asking. It never changes what a
  command does.

One verb worth knowing early: {{cmd:set}} folder-width 25 (or
preview-width, or ai-panel-width) resizes a pane for this session only —
never a round trip to the daemon, and back to 20, 40 and 30 percent the next
time the TUI starts. folder-width and preview-width each take 10 to 60, and
their sum may not exceed 90, so the message list always keeps a tenth of the
screen; ai-panel-width takes 15 to 60, because under 15 the panel cannot
hold a summary line and over 60 the list underneath it stops being usable.
An out-of-range percentage, a value that is not a whole number, or a name
that does not exist says so on the command line rather than doing nothing.
The folder and preview columns narrow on their own as the terminal does —
under 100 columns the preview goes, under 60 the folders go too, and both
widths are measured after the AI panel has taken its share, so a 120-column
terminal with the panel open leaves the panes 84 columns and no preview.
{{cmd:set}} is for taste, not for making a narrow terminal fit.

The three other doors out of the message list are [[search-vs-finder]] for
finding mail, [[bulk]] for acting on more than one message, and this manual —
{{keys:manual}} from anywhere, described in [[manual]].

## The bottom of the screen

The last row is zones, not a sentence, and only one of them moves:

- The mode, always in the same columns — one per layer, so a picker says
  `-- PICK --` and a confirmation says `-- CONFIRM --` rather than both looking
  like the message list.
- The account, the open folder, and how many of its loaded rows are unread. That
  last figure counts what this client has fetched, not the whole folder: no RPC
  in the API reports a folder's unread total, and a number labelled as one would
  be wrong by however much of the folder is not on screen.
- The message — whatever just happened. The only zone that flexes, which is why
  a two-hundred-character rejection from a mail server no longer pushes
  everything after it off the row.
- Four daemon indicators, labelled sync, idx, ai and $ — seven columns each,
  which is room for a glyph and a short name and none for the detail behind
  it. Each carries a glyph as well as a colour, so a monochrome terminal still
  tells a paused subsystem from a broken one: `✓` fine, `↻` working,
  `‖` paused, `·` switched off in config, `!` past a soft limit, `✗` blocked
  or unreachable, `?` not yet asked.
- `⧗3` when three requests *you* made are outstanding. The five-second poll
  behind the indicators deliberately does not count here — a marker that was
  always on would tell you nothing.
- Whatever is half-typed towards a binding, count included.

Narrow the terminal and the informative zones go first — the indicators, then
the account and folder, each dropped as soon as keeping it would leave the
message fewer than 24 columns. The mode, the message, the busy marker and the
pending keys never go: those four are the ones the keyboard's behaviour
depends on.

## What none of these numbers read from config

The layout is compiled in rather than configured. Nothing in the
[[config-file]] describes this screen and no RMAIL_ variable reaches it, so
every start begins at the same 20 percent folders, 40 percent preview and 30
percent AI panel, with the preview dropped under 100 columns and the folders
under 60 columns. {{cmd:set}} moves those three widths until you quit, and
there is nowhere to write them down.

The five-second heartbeat has no knob either. One tick is four local reads
over a Unix socket, which costs the daemon almost nothing and is well inside
the time you would spend wondering whether the indexer had stopped. Three of
its four answers are what {{cmd:sync status}}, {{cmd:index status}} and
{{cmd:ai status}} print in full, which is where to look when a glyph is not
enough. The fourth, the $ indicator, has no report of its own: the verb it
would expand into, ai budget status, is not in this build, so the glyph and
its colour are all the bar itself can tell you about spend — {{cmd:ai cost}}
prints the day's figure against its cap, and [[ai-cost]] is where the cap
comes from.

## Where to read next

- [[daemon]] — what is running behind the screen, and what happens when it
  is not.
- [[typing]] — the keys that only exist while something is asking you a
  question.
- [[archive]] — what the four message-moving keys actually do to your
  server.
