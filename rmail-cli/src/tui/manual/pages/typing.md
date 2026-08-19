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

{{keys:input.submit}} and {{keys:input.backspace}} are the same two keys for
a plain text field — a note, a tag name — where there is nothing to complete
against.

## A yes or no

{{keys:confirm.accept}} confirms; n, or Esc, does not. Only actions that
cannot be undone ask, which in practice means delete — see [[archive]]. A
client that asked about everything would train you to answer yes without
reading, which is the state in which a confirmation stops protecting
anything.

## A half-typed chord

Press the first key of a chord and a strip appears along the bottom saying what
the next key can do. It appears at once rather than after a pause: a chord that
is complete has already run by then, so nothing half-typed is waiting to find
out what you meant. [[keys-toml]] has the longer version, including what a
struck-through entry in that strip means.

The same strip serves {{keys:command}}: with a `:` line half-typed it lists the
verbs that could come next instead of the keys.

## The way out is always the same

Esc backs out of the innermost thing, in every one of these layers, and no
config file can take it away. [[modes]] is the generated table of which layer
falls through to which.
