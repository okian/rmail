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

--with-ai is the one door through that wall, and it opens only for
--archive-format json: it attaches the stored AI summaries and the applied
tag names for each message, beside the raw bytes in the JSON document. Notes
are not among them, and no flag exports one. It copies what earlier passes
already produced and never calls a model, so it costs nothing and returns
the same bytes twice.

## Where an archive lands, and what you get by naming nothing

An export lands where -o names it and nowhere else. That flag is required
and has no default, so no archive is ever written to a directory you did not
type; there is no export table in the [[config-file]] and therefore no
RMAIL_EXPORT__ override to hunt for either. The selection has no default
either — omit both the query and --thread and the verb refuses, because an
export that silently widened to the whole mailbox is the one mistake here
that cannot be taken back.

Name no format and you get mbox: one file, with mboxrd quoting so the
original bytes stay recoverable. Writing to - sends that single document to
stdout, which mbox and json accept and maildir and eml refuse, since one
file per message is a directory and not a stream. The limit is 0, meaning no
limit, so the default is the whole selection — which is what a pre-rebuild
archive wants. And the two single-file formats refuse a path that already
exists unless you pass --force, because replacing yesterday's archive with
today's shorter one is not something this verb should do quietly. Both
printed defaults, mbox and 0, sit beside their flag in mail export --help,
and the two refusals are spelled out there beside -o and --force.

## The narrower habit

Before a bulk action wider than a screen, look at what it will hit first —
the selection is visible, which is most of the point of making one. See
[[bulk]].
