# Practice: export before you rebuild

Take an export before any operation whose name contains the word rebuild,
reset or purge — even though the index is derived and the mail is not.

## Why

The export costs minutes and the confidence it buys is what lets you run the
destructive command now instead of postponing it for a month.

## What an export is

```
mail export 'in:INBOX after:2025-01-01' --out backup.mbox
mail export --thread <id> --archive-format eml --out thread/
```

{{capability:ExportExport}} writes an archive rather than a search result:
every message the selection matches, in a deterministic order, with its raw
RFC822 preserved byte for byte. That last part is what makes it a backup —
what comes out is what arrived, not a rendering of it.

The shape flag is spelled --archive-format, not --format: --format is the
global output flag every verb carries, and an export writing a JSON status
report where an mbox was meant is the exact accident that spelling avoids.

## What it does not carry

Tags, notes and AI artifacts live in the local database, not in the message
bytes. An export is your mail, not your workspace. If what you are about to do
could touch the database rather than the index, copy the database file too.

## The narrower habit

Before a bulk action wider than a screen, look at what it will hit first —
the selection is visible, which is most of the point of making one. See
[[bulk]].
