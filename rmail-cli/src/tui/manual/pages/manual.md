# Reading the manual

{{keys:manual}} opens this manual from anywhere. {{keys:help}} is the other
half of it: a live list of the bindings in force, generated the same way
[[keys]] is.

## Moving

- {{keys:cursor.down}} and {{keys:cursor.up}} move a row at a time, and a
  count works here too.
- {{keys:cursor.page-down}} and {{keys:cursor.page-up}} move a screenful,
  which on a page this long is the movement you want. A count means pages.
- {{keys:cursor.top}} and {{keys:cursor.bottom}} jump to the ends.
- Enter follows the link on the row under the cursor.
- {{keys:manual.back}} goes back to the page you came from,
  {{keys:manual.forward}} forward again. This is vim's jump list, and it
  remembers where you were on each page as well as which page it was. It
  holds 64 positions in each direction and drops the oldest rather than
  growing, which is deeper than a reading session goes.
- {{keys:back}} or Esc leaves the manual and puts you back on the screen you
  opened it from.

Ctrl-I and Tab are bound to the same thing on purpose: on a terminal without
the kitty keyboard protocol they are the same byte, so Ctrl-I alone would be
a binding most terminals could never deliver.

## Searching

- {{keys:search}} searches this page. Enter jumps to the first match and
  leaves the rest highlighted; {{keys:manual.next-match}} and
  {{keys:manual.prev-match}} step through them. Esc clears the highlight, and
  a second Esc leaves the manual.
- {{keys:manual.grep}} searches every page and lists the hits. Enter on a hit
  opens that page with the pattern still highlighted, so
  {{keys:manual.back}} takes you back to the list. It stops at 500 matching
  lines and says that they are the first 500: a one-character pattern
  matches most of the manual, and a list nobody can walk is not a better
  answer than a truncated one that says it was truncated.
- {{cmd:helpgrep}} names that same search, with the pattern as its argument.
  It is a name rather than something to type here — see below.

Both searches fold case, and neither reads an empty pattern as a match for
everything: it finds nothing, which is the state the box is in before you
type.

## How the manual is laid out

[[start-here]] is the index, and it groups every page into five kinds:

- Getting started, which is this page, [[tour]], [[typing]], [[daemon]] and
  [[offline]] — enough to use the client without looking anything else up.
- Concepts, one per thing that is easy to get wrong: what
  [[archive]] does to your server, what [[grounded]] means, where the money
  goes in [[ai-cost]].
- Worked examples, which are transcripts rather than explanations. Each one
  starts from a situation and ends with the commands that resolved it.
- Practices, each a single habit followed by the one sentence that justifies
  it. If the reason does not convince you, the habit is not for you.
- Reference: the four generated pages below, plus [[keys-toml]],
  [[config-file]], [[provider-settings]] and [[troubleshooting]].

## What is written and what is derived

Prose pages are authored and compiled in. Four pages are not written at all:
[[keys]], [[commands]], [[modes]] and [[capabilities]] are read out of the
keymap this session loaded, the verb registry, and the capability table, every
time you open them. Rebind a key and [[keys]] says so within the second, with
no restart and nothing to regenerate.

Prose names keys and commands the same way, rather than spelling them out.
A page writes an action id or a verb path in a marker and the renderer
resolves it against the live registry — so a page cannot go on naming a
binding that was rebound, or a command that was renamed, without a test
failing:

```
{{keys:message.archive}}      -> the chords that archive, right now
{{cmd:message archive}}       -> the verb, checked against the registry
{{capability:MailMove}}       -> the RPC behind it
[[keys]]                      -> a link, labelled with that page's title
```

There is deliberately no inline bold or code form. The markers above are the
inline vocabulary, and the reason is that they are checked: a dangling link,
an unknown action id, a verb that no longer exists and a mistyped capability
name are all build failures rather than stale text nobody notices. So are the
mail command lines these pages show in fences, which are reconciled against
this binary's own argument parser.

## What a colon spelling is

A verb rendered as :message archive is both a name and something you can
type. The key namespace and the verb grammar are one vocabulary — a dot and a
space are the same separator — so every action has a name in this form, and
{{keys:command}} is the line that reads one.

A verb with no arguments dispatches to exactly the same code its key does, so
a command and its binding cannot drift. What the line adds is everything a
key cannot carry: an argument, a range, and a trailing bang. See [[tour]].

## Pages that reach the daemon

A page's footer lists the RPCs behind whatever commands it names — derived
from the page's own text, so it cannot claim one it never mentions. This page
names {{cmd:helpgrep}}, which reaches nothing at all: the manual is compiled
in, and every generated section is read from this process. It is the one
screen that works identically with the daemon stopped.

## What is fixed here, and what you can change

The manual has no settings of its own. There is no manual table in the
[[config-file]] and no environment variable that changes a page — the prose
is compiled into this binary, and the numbers above are constants rather
than knobs. Pages wrap at 78 columns rather than at your terminal's width,
for the reason man fixes its own: a paragraph reflowed to 200 columns is
unreadable, and a document whose line count moves with the window is a
document whose cursor, scroll offset and hit line numbers all move with it
too. Fenced blocks are not wrapped at all, which is why the marker examples
above keep the spacing they were written with.

Two things outside the manual do change what you read here. The chords come
from the keymap, which is $RMAIL_KEYS when that is set and otherwise
keys.toml beside the master config — $HOME/.config/rmail/keys.toml on an
install that has moved neither. It is re-read once a second, which is the
second this page keeps promising, and [[keys-toml]] is that file's own page.
The colours come from the theme, dark unless RMAIL_THEME or mail tui --theme
names light, mono or high-contrast; an unrecognised name falls back to dark
and says so in the status line rather than refusing to start.
