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
