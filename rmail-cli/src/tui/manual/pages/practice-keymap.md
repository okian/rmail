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
every binding in force, grouped by the layer that declares it.

```
mail keys list --mode normal
mail keys set '<c-j>' cursor.down --mode normal
```

## What you cannot take

Esc and Ctrl-C are global and not rebindable. A config file that could remove
the way out of a modal screen is a config file that can lock somebody in.

## Unbinding is a real operation

An empty string unbinds. That matters more than it sounds: the file is a
delta against the built-in bindings, so removing your line restores the
default rather than leaving a hole. [[keys-toml]] is the file's own page.
