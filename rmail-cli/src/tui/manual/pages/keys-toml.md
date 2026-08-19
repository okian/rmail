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
