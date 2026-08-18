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
- {{cmd:helpgrep}} does the same thing from the command line, with the
  pattern as its argument.

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
name are all build failures rather than stale text nobody notices.

## Pages that reach the daemon

A page's footer lists the RPCs behind whatever commands it names — derived
from the page's own text, so it cannot claim one it never mentions. This page
names {{cmd:helpgrep}}, which reaches nothing at all: the manual is compiled
in, and every generated section is read from this process. It is the one
screen that works identically with the daemon stopped.
