# keys.toml

The key bindings live in a file, not in the daemon. That is why rebinding
works with nothing running, and why [[keys]] can change within a second of a
save with no restart.

## Where it is

$RMAIL_KEYS, if set. Otherwise keys.toml beside the master config file — so
pointing $RMAIL_CONFIG at a second profile moves both, which is what a second
profile means. With neither variable set that is ~/.config/rmail/keys.toml,
and on a fresh install no such file exists: every binding you have comes from
the built-in map compiled into the binary. mail keys list prints the path it
read as its own first line, so it answers which of the two variables won
without you working it out.

At most 256 kilobytes of the file is read. The whole built-in map is under
two kilobytes, so the cap is three orders of magnitude of headroom, and it
exists because the path is itself a setting — a typo can aim $RMAIL_KEYS at
something that is not a config file at all, and reading that forever, once a
second, behind a TUI that never says why is the failure worth ruling out.

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
be named; [[modes]] lists them and says which falls through to which. There
are nine — normal, viewer, visual, insert, prompt, menu, pick, confirm and
help — and global, the layer that holds Esc and Ctrl-C, is not one of them. A
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

Four keys is the longest chord any layer can bind, and nothing raises it: a
fifth key makes the line a refusal at load time rather than a binding. The
bracket names are a closed list — esc and escape, cr and enter and return,
tab, bs and backspace, up, down, space, and lt for a literal < — plus c- with
one character after it. Those names are matched without case, so <C-P> and
<c-p> are one binding; a bare character keeps its case, so G and g are two.

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

Both set and unset write to the normal layer unless --mode names another one,
and list takes the same flag to print one layer instead of all nine. What
unset removes is a line this file added; where there is no such line it
refuses rather than writing the empty string, because suppressing a built-in
binding is the other request and one you make by hand.

## Rebinding from the ? overlay

{{keys:help.rebind}} on a highlighted row opens the command line pre-filled
with {{cmd:keys set}} for that row's own chord and action — edit it and press
Enter, or Esc to back out. `keys set` is the same edit `mail keys set` makes,
reached without leaving the TUI to a shell:

```
:keys set <c-j> cursor.down             bind, in normal mode
:keys set --mode=viewer j cursor.down   bind in another mode
```

No shell quoting here — the `:` line is typed straight into the TUI, not a
shell, so `<` and `>` need none of the protection they need on a command
line. `--mode` takes its value joined with `=`, the one spelling this
grammar's flags accept.

The mode defaults to `normal`, the same default `mail keys set --mode` has,
so a row from that mode's own chain never needs to spell it out.

## Failure behaviour

- A file that stops parsing does not clear your bindings. The error goes to
  the status line and the previous keymap keeps working, because a typo
  mid-edit must not leave you holding a TUI whose keys have all changed.
- A missing file is not an error. It is the state every install starts in.
- Esc and Ctrl-C cannot be bound or unbound at all, and no chord may begin
  with either. They are the two bindings in the global layer, which is the
  one layer this file may not name, and a chord starting with Esc would make
  a bare Esc merely pending — one keystroke from a mode nobody can leave.

## Reload

The file is re-read once a second and compared by content, not by
modification time — mtime has one-second granularity on real filesystems, so
two edits within the same second are invisible to it, and two edits in a row
is exactly what trying a binding looks like.

The poll runs on a thread of its own, so a slow filesystem stalls no request
in flight, and a second is the longest someone editing a binding in the next
window will believe the file was read. Deleting the file counts as a change:
the next poll restores the built-in bindings rather than keeping the ones it
used to define. The first read, at startup, says nothing — a status line
announcing that the keymap loaded would stamp on the boot progress you are
actually waiting for — but an error is announced whenever it happens,
startup included.

## Bindings that can never be typed

There is no timeout here. An exact match fires immediately, so `g` bound in one
mode makes `gg` in that same mode untypeable — and that case is refused outright,
when the file loads or when `:keys set` writes it.

The case that *cannot* be refused is across layers. `Viewer` inherits `Normal`,
so a `g` bound in the viewer buries `Normal`'s `gg` there and nowhere else:
neither binding is illegal, neither mode can see the other's, and the result is a
chord you can write, save and never fire.

{{cmd:keys check}} lists them: the mode you meet it in, the binding that never
fires, and the one that fires instead. It also runs on every load — including the
first — and the status line says how many it found, because a chord that silently
does nothing reads as a broken client rather than a shadowed binding.

The fix is always the same: unbind the shorter one, or move it.

## The new key names

`<left>`, `<right>`, `<home>`, `<end>`, `<pageup>` and `<pagedown>` are bindable.
They were not before — the terminal's own key events for them were dropped before
they reached the keymap at all, so a binding on `<home>` was not merely unbound,
it was unwritable. `<pgup>`, `<pgdown>` and `<pgdn>` are accepted as aliases,
because somebody who writes vim's spelling means the same key.

## The numbers here, and which of them are settings

Two are, and neither belongs to the environment overlay the rest of rmail's
settings take — the RMAIL_SYNC__INTERVAL shape. $RMAIL_KEYS names the file
and $RMAIL_CONFIG moves it by moving the directory it sits in, and both are
read before the config system exists, which is why there is no keys table in
the [[config-file]] and no environment variable for an individual binding. A
binding is a line in this file or it is a built-in.

The rest are constants in the binary, with no key and no variable behind
them: four keys in the longest chord, 9,999 as the largest count typed in
front of one, 256 kilobytes as the most of this file that is read, one second
between polls. The first two bound a key you can hold down, and what a stuck
key is allowed to consume does not vary from one person to the next.

The bindings themselves default to the built-in map, which is what you are
running until this file says otherwise. [[keys]] is that map as it stands
after the file has been applied, {{keys:help}} is the same list inside the
client, and mail keys list is the command-line form.
