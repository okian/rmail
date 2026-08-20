# The leader map

`<space>` opens a page of commands grouped by domain. Press it and the band along
the bottom shows what the next key can be; press that and it shows what the one
after can be. Nothing is memorised in advance — the band is the map.

## The groups

```
  <space>a   ai              <space>s   search and saved
  <space>t   tags            <space>o   outbox
  <space>r   rules           <space>x   what is in a message
  <space>d   the daemon      <space>n   notes
  <space>c   configuration   <space>g   go somewhere
  <space>w   webhooks/hooks  <space>h   help
```

Bound in the message list, and live in the viewer and in a visual selection too —
those two inherit the list's layer, so the chords are bound once rather than three
times. Three copies would be three things to keep in step in your own
`keys.toml`.

## What the band calls a group

The label under a prefix is *derived*, never written down: it is the longest
leading part the members' names share. `<space>a` holds `ai.panel`, `ai.quick`
and `ai.status`, so the band says `ai…`. `<space>d` holds `sync.status`,
`index.status` and `ai.status` — three services, no shared name — so the band says
`3 commands` instead of inventing one.

That is the point rather than a shortfall. A hand-written group table is a table
that goes stale the first time a binding moves, and a band that said `daemon…`
over a group whose members had all been rebound elsewhere would be lying
confidently. See [[typing]] for the band itself.

## What a leader key can and cannot reach

Every member runs a `:` verb that needs no arguments and acts on what is on
screen. That is what makes a domain bindable at all.

A verb that needs *words* — an address, a query, a tag name, a note — has nothing
a keystroke could supply, so it is not in the map. Those live on the `:` line, and
the [[settings]] screen will put one there for you with the cursor after it.

So `<space>tl` lists your tags and `<space>ts` asks the model to suggest some, but
adding a tag is `:tag add <name>`. Nothing is hidden; the boundary is just what a
single keypress can honestly mean.

## If you already bind `<space>` or `:`

Two notes for anyone with an existing `keys.toml`.

**`<space>`** is now the first key of about thirty built-in chords. If your file
binds `<space>` on its own, that binding still wins — your file is applied over
the built-ins, and a shorter binding shadows the longer ones under it. What you
lose is the leader map, and {{cmd:keys check}} will say so: it lists every chord
that can no longer be typed and the binding that fires instead. If you want both,
move your own binding to another key.

**`:`** opens the command line and always has. `palette` is still a working alias
of `command` — the same action under two names, kept because task 85 shipped it —
so a file binding either keeps working exactly as it did. Nothing was removed or
rebound to make room for the leader: every default binding this client has ever
shipped still resolves to the same action.

## Where to read next

- [[typing]] — the band, the counts and the modes.
- [[keys-toml]] — rebinding, and the check that finds chords nobody can type.
- [[settings]] — the screen for the switches a keystroke cannot carry.
