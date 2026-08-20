# The settings screen

{{cmd:settings}} is every switch this build has, in one place. `gs` opens it from
the message list and `s` opens it from any report — a table of what a subsystem is
doing and the switches behind it are the same subject.

`j` and `k` move between fields, {{keys:focus.toggle}} moves to the next
section — the same key that switches panes on the message list, because it means
the same thing — `<enter>` acts on the highlighted field, and `<esc>` closes it.
`:settings ai` opens straight to a section.

## Every field is a `:` line

There is no private path from this screen to the daemon. Each field's write is an
ordinary `:` command — the same one you could type — which has three consequences
worth knowing.

A field cannot do something a typed line cannot. It cannot reach a capability no
verb reaches, and it cannot skip a confirmation a verb asks for: `<enter>` on
Index › rebuild asks `[y/N]` exactly as `:index rebuild` does, because it *is*
`:index rebuild`.

Every write goes into your command history, so what the screen did is visible
afterwards in the same place everything else you typed is.

And it means the screen is testable without a daemon at all: the test suite
asserts that `<enter>` on each field produces the expected line, with nothing
running.

## It does not show current values

A toggle here does not know whether the thing is currently on. That is deliberate.
Asking would mean a read per field every time the screen opened, a value that goes
stale between reads, and a screen that could not be checked without a daemon.

What it does instead: every section's first field is the *report* that answers
"what is it now". Press `<enter>` on it. So a section reads as "here is the state,
and here are the switches" — and the state comes from the surface built to say it,
which knows how to draw a soft cap differently from a hard one and a paused queue
differently from a stopped one.

## What a field can be

A **toggle** has two states and `<enter>` moves to the other one — Sync › fetching
runs `:sync pause` or `:sync resume`. A **choice** cycles through several — AI ›
backend runs one of the three `:ai provider set` lines. An **action** just runs —
Index › drain the queue.

A **number** opens a form. AI › caps is the example, and the form exists because
storing a budget *replaces* it: a line naming one cap would clear the rest, so the
form reads what is in force first and applies all of it. See [[ai-policy]].

A field that needs **words** — an address, a token label, a chord, a query — puts
the verb on the `:` line and leaves the cursor after it. It runs nothing, because
there is nothing for the screen to run: only you have the words. That is why
Accounts › add an account opens `:account add ` rather than doing something.

## Two kinds of read-only

Some settings this screen can show and not change, and the two reasons are
different facts.

**Config file only** means it lives in `rmail.toml` and *nothing* changes it over
the wire — hooks and notification thresholds are the examples, and both protos say
why: a setting that lives in your config file must not also live in a database the
daemon then has to keep in sync with it. `<enter>` renders the exact block, names
the file, and offers to open it so it can be copied. See [[automation]].

**No RPC** means this build cannot change it at all, and the field says what would
have to exist. Safety › when a flag withholds actions is one: it is a severity in
the config file and there is no verb that renders that block yet.

## Keys does not go through the daemon

Settings › Keys writes `keys.toml` directly. It has to: a keymap you cannot fix
because a socket is missing is a keymap you are stuck with, and putting a network
hop in front of a local file would be exactly that. See [[keys-toml]].

## Where to read next

- [[keys-toml]] — the file the Keys section writes, and its grammar.
- [[automation]] — the config-file-only settings, and why they are.
- [[ai-policy]] — the form AI › caps opens, and why it is a form.
- [[reports]] — the screen every section's first field opens.
