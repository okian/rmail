# Practice: triage in one pass

Walk the unread list once, top to bottom, and give every message exactly one
of two dispositions: gone, or flagged for later. Do not reply during the pass.

## Why

A triage pass and a reply session need different states of mind, and mixing
them turns a five-minute sort into an hour.

## What that looks like

- {{keys:message.toggle-read}} marks read, which is the disposition for
  anything you have now seen and do not need to act on.
  {{cmd:message toggle-read}} is the same thing by name.
- {{keys:message.toggle-flag}} is the only "later" bucket you need. A second
  bucket that means a slightly different kind of later is a bucket you will
  stop maintaining.
- Archive the obvious noise in ranges rather than one row at a time — see
  [[bulk]].

Both flag actions apply one intent across a mixed selection rather than
toggling each row, so a range is as predictable as a single message.

## The tell that you are doing it wrong

If the pass keeps stalling on a message, the message needs a decision you do
not have yet — flag it and move on. If the same kind of message stalls you
every week, it needs a rule instead: [[rule-from-mistake]].

## Where these keys and their limits come from

The chords above are the built-in bindings, and [[keys-toml]] is the only
place they change; nothing else about the pass is configuration. One action
takes at most 100 messages at a time, and past that the range is refused
with both counts rather than truncated to a hundred rows you would then have
to identify — the cap is compiled into the binary, with no config table and
no environment override behind it, and [[bulk]] carries the reasoning.
{{keys:message.archive}} has no destination setting either: it takes the
first of Archive, Archives and All Mail that the account has and that is not
the folder you are looking at, matched on the folder's last path segment,
and refuses the key on an account with none of the three — see [[archive]].
