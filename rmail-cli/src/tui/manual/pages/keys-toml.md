# keys.toml

The key bindings live in a file, not in the daemon. That is why rebinding
works with nothing running, and why [[keys]] can change within a second of a
save with no restart.

## Where it is

$RMAIL_KEYS, if set. Otherwise keys.toml beside the master config file — so
pointing $RMAIL_CONFIG at a second profile moves both, which is what a second
profile means.

## It is a delta, not a replacement

The file states what is different from the built-in bindings. A user who wants
Ctrl-J to move down writes one line and keeps everything else:

```toml
[normal]
"<c-j>" = "cursor.down"   # bind
"d"     = ""              # unbind: d now does nothing
```

The alternative — the file being the complete map — means every binding a new
release adds is invisible to everyone who ever customised anything, which is
how a keymap file becomes a thing people stop upgrading.

## Sections are layers

One table per layer, named by the layer's id. Only the configurable layers may
be named; [[modes]] lists them and says which falls through to which. A
binding added to the wrong layer is the most common mistake here — see
[[practice-keymap]].

## Chords

Vim notation. A bare character is itself; angle brackets name a key or a
modifier combination:

```
j        gg       G        ?
<c-p>    <esc>    <enter>  <tab>    <bs>    <up>
```

Quote them in a shell: the angle brackets are redirections.

## A chord fires as soon as it is complete

There is no timeout anywhere in this. A chord that is complete runs at once,
even when a longer binding starts with it — so binding `g` in a layer that
already has `gg` does not make `g` wait to find out which you meant; it makes
`gg` a binding the keyboard can never deliver.

Within one layer that is refused outright: an edit that would make either of
two bindings unreachable is rejected rather than written. Across layers it
cannot be, because the edit is legal on its own terms — `g` in the viewer is a
reasonable thing to want, and the fact that it kills the `gg` the viewer
inherits from `normal` is a consequence of the chain rather than of that line.
So it is reported instead: the band along the bottom of the screen draws such a
binding struck through, with a note saying it cannot be typed.

## The band along the bottom

Press part of a chord and a strip appears immediately, listing what the next
key can do: the keys that complete something, the ones that open more, and Esc
and Ctrl-C, which are in every one of them. A count on its own does not raise
it — `3` is a repeat waiting for a command, and every binding is still
available, so a list of all of them would say nothing.

Group names in that strip are derived from the ids of what is under them, never
written down anywhere: two bindings under one key whose actions are `ai.panel`
and `ai.quick` label their group `ai`. Rebind either and the label follows,
because there is no second copy of it to go stale.

## Editing it from the command line

```
mail keys list                       every binding, by layer
mail keys actions                    every action id a binding can name
mail keys set '<c-j>' cursor.down    write one line
mail keys unset '<c-j>'              remove that line
```

An edit rewrites one line and leaves every other byte — comments included —
where it was, then re-parses the result and refuses to write unless the only
binding that changed is the one asked for.

## Failure behaviour

- A file that stops parsing does not clear your bindings. The error goes to
  the status line and the previous keymap keeps working, because a typo
  mid-edit must not leave you holding a TUI whose keys have all changed.
- A missing file is not an error. It is the state every install starts in.
- Esc and Ctrl-C cannot be bound or unbound at all.

## Reload

The file is re-read once a second and compared by content, not by
modification time — mtime has one-second granularity on real filesystems, so
two edits within the same second are invisible to it, and two edits in a row
is exactly what trying a binding looks like.
