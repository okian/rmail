# Reading the manual

{{keys:manual}} opens this manual from anywhere. {{keys:help}} is the other
half of it: a live list of the bindings in force, generated the same way
[[keys]] is.

## Moving

- {{keys:cursor.down}} and {{keys:cursor.up}} move a row at a time, and a
  count works here too.
- {{keys:cursor.top}} and {{keys:cursor.bottom}} jump to the ends.
- Enter follows the link on the row under the cursor.
- {{keys:manual.back}} goes back to the page you came from,
  {{keys:manual.forward}} forward again. This is vim's jump list, and it
  remembers where you were on each page as well as which page it was.
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
  {{keys:manual.back}} takes you back to the list.
- {{cmd:helpgrep}} names that same search, with the pattern as its argument.
  It is a name rather than something to type here — see below.

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
  [[config-file]] and [[troubleshooting]].

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

A verb rendered as :message archive is a name, not an instruction to type it.
The key namespace and the verb grammar are one vocabulary — a dot and a space
are the same separator — so every action has a name in this form whether or
not anything reads one from a prompt.

This build has no command line that does. {{keys:palette}} runs a verb by
name, and every verb with a key has that key; the typed line, its arguments
and its vim-style ranges are the next thing to land, and these pages name
verbs in the spelling it will use.

## Pages that reach the daemon

A page's footer lists the RPCs behind whatever commands it names — derived
from the page's own text, so it cannot claim one it never mentions. This page
names {{cmd:helpgrep}}, which reaches nothing at all: the manual is compiled
in, and every generated section is read from this process. It is the one
screen that works identically with the daemon stopped.
