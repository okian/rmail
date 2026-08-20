# Practice: bind in the layer you stand in

When a key does the wrong thing, rebind it in the specific layer where you
pressed it, not in the one that covers everything.

## Why

Overlay layers stop at global rather than falling through to the message
list, so a binding added in the wrong layer either does nothing or reaches
mail behind a screen that is covering it.

## How to find the layer

The mode is derived from what is on screen and is shown to you, and [[modes]]
is the generated table of which layer falls through to which. [[keys]] lists
every binding in force, grouped by the layer that declares it. Of the nine
layers a file may name, only viewer and visual reach normal on their way to
global; insert, prompt, menu, pick, confirm and help each stop at global
directly, with nothing in between.

```
mail keys list --mode normal
mail keys set '<c-j>' cursor.down --mode normal
```

Given no --mode, the first of those walks all nine layers in turn. Given one,
it prints that layer's effective bindings rather than the ones that layer
declares, so a listing of viewer has normal's j folded into it. Which layer a
binding comes from is the question that view does not answer, and the reason
the key reference is grouped the other way.

Name the layer on the second line as well. Both mail keys set and mail keys
unset default --mode to normal, so a chord meant for an overlay lands on the
message list when the flag is left off — and neither command can do better
than that default, because the layer you were standing in is derived from a
screen belonging to another process.

## What you cannot take

Esc and Ctrl-C are global and not rebindable. A config file that could remove
the way out of a modal screen is a config file that can lock somebody in.

## Unbinding is a real operation

An empty string unbinds. That matters more than it sounds: the file is a
delta against the built-in bindings, so removing your line restores the
default rather than leaving a hole. [[keys-toml]] is the file's own page.

That file is keys.toml beside the master config — with neither $RMAIL_KEYS
nor $RMAIL_CONFIG set, ~/.config/rmail/keys.toml — and on a fresh install it
does not exist, so every binding this page is about comes from the built-in
map until mail keys set writes the first line. No table in config.toml
holds a binding.
