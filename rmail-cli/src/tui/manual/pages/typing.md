# Typing, choosing and confirming

Three kinds of question interrupt the message list, and each brings its own
small set of keys. They are separate layers rather than one, because a layer
where keys are text must not also be a layer where a key can archive
something.

## A list to choose from

A folder picker, the quick menu, a result list: the cursor keys come back,
and {{keys:menu.accept}} uses the highlighted row. The folder picker has an
accept of its own — {{keys:pick.accept}}, the same key in a layer of its own
— because what it returns is a destination rather than a row, and the two
answers go to different places.

These layers restate the movement keys rather than inheriting them from the
message list. Inheriting would mean a key bound to delete still reaching the
mail behind the overlay that is covering it.

## A line to type

The search line and every other prompt bind only what cannot be typed:
{{keys:prompt.accept}} to run what is there and {{keys:prompt.complete}} to
complete the operator being typed. Everything else falls through as text,
which is what makes searching for the word q possible at all.

Backspace, the two arrow keys, {{keys:cursor.page-down}} and
{{keys:cursor.page-up}} are the rest of it — seven chords in that layer and
nothing more — and they move and page the hits under the line rather than
the line itself. j and k are deliberately not among them: a control chord is
not text and a letter is.

{{keys:input.submit}} and {{keys:input.backspace}} are the same two keys for
a plain text field — a note, a tag name — where there is nothing to complete
against.

Every text field in the client stops at 512 characters, which is longer than
any address or subject a person types. The key after that does nothing —
no error, no bell — because the cap is there to stop a key leaned on for a
minute growing a string without limit, not to reject what you meant to ask.
Digits are text in both of these layers, so the 3 in from:alice3 is part of
what you are searching for rather than a repeat count, and for the same
reason no multi-key chord can be bound in either of them: the first key of a
chord is held back, and a held-back keystroke inside a text field is
indistinguishable from a dropped one.

## A yes or no

{{keys:confirm.accept}} confirms; n, or Esc, does not, and neither does N or
the q that backs out of a list. Only actions that cannot be undone ask,
which in practice means delete — see [[archive]]. A client that asked about
everything would train you to answer yes without reading, which is the state
in which a confirmation stops protecting anything.

## A half-typed chord

Press the first key of a chord and a strip appears along the bottom saying what
the next key can do. It appears at once rather than after a pause: a chord that
is complete has already run by then, so nothing half-typed is waiting to find
out what you meant. [[keys-toml]] has the longer version, including what a
struck-through entry in that strip means.

Four keys is the longest chord any layer can bind, so no more than three are
ever waiting. Press a fourth and the sequence has to resolve: it runs, or it
drops the key that has been waiting longest and re-resolves the rest, so a
mistyped prefix costs you the prefix and not the keystroke after it. A count
typed in front of a chord saturates at 9,999 rather than accumulating
digits, and a count in front of a verb on the command line stops at the same
number — a held-down digit is a stuck key, not a request to allocate.

The same strip serves {{keys:command}}: with a colon line half-typed it
lists the verbs that could come next instead of the keys.

## The way out is always the same

Esc backs out of the innermost thing, in every one of these layers, and no
config file can take it away. Esc and Ctrl-C — which quits — are the two
keys keys.toml may neither bind nor unbind, and no binding anywhere may
begin with one of them: a chord starting with Esc would make a bare Esc
merely pending, which is one keystroke away from a mode nobody can leave.
[[modes]] is the generated table of which layer falls through to which.

## What each layer binds, and where to change it

The five layers this page is about, and the global one underneath all of
them, bind this much on a fresh install and nothing else:

```
insert     <enter> <bs>
prompt     <enter> <bs> <tab> <up> <down> <c-d> <c-u>
menu       j k <down> <up> gg G <c-d> <c-u> <enter> x u r : / q
pick       j k <down> <up> gg G <c-d> <c-u> <enter> q
confirm    y Y n N q
global     <esc> <c-c>
```

Insert and confirm have no page keys, and that is an absence of anything to
page rather than an omission: a one-line field and a yes or no question have
no rows and no scroll offset, so the key would be a documented binding that
does nothing wherever it was pressed.

Those names are the section names too — [[keys-toml]] has one table per
layer, and adding a binding to the wrong one is the mistake
[[practice-keymap]] is about. Global is the layer that file may not name,
because Esc and Ctrl-C are what it holds. [[keys]] is the same table
generated from the map actually in force rather than from the built-in one,
and {{keys:help}} is that list inside the client.

Three numbers put a ceiling on what you can type, and none of them has a
config key or an environment variable behind it: 512 characters in a text
field, four keys in the longest chord, 9,999 as the largest count. Each of
them bounds a key somebody is holding down, and what a stuck key should be
allowed to consume does not vary from one person to the next.
