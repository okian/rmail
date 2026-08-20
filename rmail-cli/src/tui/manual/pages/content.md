# Reading what is in the mail

Four families of verb over what a mailbox contains: what is attached and what is
inside it, what a message *means* (events, tasks, structured data, links), what
the mailbox as a whole says about the people writing to you, and how to get any
of it out.

## Attachments

{{cmd:attach list}} is what is attached to the message you have open. It reaches
no daemon at all — the parts came back with the message — so it is the cheapest
thing on this page and the right first step.

{{cmd:attach tables}} pulls tables out of spreadsheets, CSVs and HTML, one report
row per table row. A spreadsheet or a CSV is parsed outright; `--model` lets a
model read what the parsers cannot, which costs money and is why it is opt-in. A
table the model inferred says so on its own row, because an inferred table is a
guess about somebody's numbers.

{{cmd:attach invoice}} reads one invoice or receipt. Every field carries a `from`
column, and that column is the point: a total a parser read out of a PDF's text
layer and a total a model inferred from a scan are not the same claim, and an
invoice report that flattened them would be inviting you to pay the second one.
An inferred invoice gets a warning row of its own.

{{cmd:attach invoices}} is everything already extracted, filtered by
`--vendor` and a window. `--format csv` gives you the document instead of rows.

{{cmd:attach ask}} asks a question of a document:

```
mail api call AttachmentService.AskAttachment '{"question":"what is the total","message_id":42}'
```

It is scoped to the open message unless `--all`, which is deliberate: retrieval
across every attachment in an account is a much larger model call, and somebody
looking at a document usually means that document. The answer's citations follow
it, and a row appears if the answer is *ungrounded* — cites nothing — because an
uncited answer from a document reader is a guess.

{{cmd:attach search}} searches inside attachments — the text of the PDFs and
spreadsheets, not their filenames. {{cmd:search attachments}} is the same verb
under the `:search` family; it belongs to both, the same way `:helpgrep` and
`:manual grep` are one thing under two names. Enter on a hit runs
{{cmd:message open}}, which is the verb every citing row here reaches for: a
digest line, an attachment hit, a saved search's result, a smart folder's member.
It is the keyboard's own `<enter>` addressed by id, which is the only thing a row
can carry.

## What a message means

{{cmd:extract events}} and {{cmd:extract tasks}} pull calendar events and to-dos
out of a message. A real `.ics` attachment is parsed; free text needs `--model`.
An item the model inferred is drawn as a warning, because a meeting time read out
of a sentence can be wrong in a way one read out of an invitation cannot. A
*cancelled* event is drawn red — it means something you may still have on a
calendar is off.

`--sink command` and `--sink webhook` deliver the items rather than only reporting
them, and delivery is idempotent: a second run says how many were "already
claimed" instead of sending them again. That claim is why these verbs are
mutations even without `--model`.

{{cmd:extract data}} runs a configured extraction schema over a message and shows
the JSON. `--refresh` re-extracts instead of answering from the cache, which costs
a model call.

{{cmd:links}} lists the links in a message, classified. Two rows matter: a
**deceptive** link — one whose visible text names a different host from where it
goes — is drawn red with the reason next to it, and the count of *tracking pixels*
is called out, because those are images whose only purpose is to tell the sender
when you opened the mail.

## What the mailbox says about itself

{{cmd:stats response-time}} is how fast you reply and who is waiting. The `note`
column is the report: `you are the delay` means the daemon judged you the slow
side of that correspondence, and `stalled` means the thread went quiet on your
turn. `--group-by mailbox` groups by folder instead of by contact.

```
mail stats response-time --since 30d
```

`--since` takes a duration — `30d`, `12w`, `6h` — and every report here names the
window it actually summarized, so a figure is never a number whose period you have
to assume.

{{cmd:digest}} is the period summarized in prose, with each line citing the
messages behind it. Enter on a line opens the first of them, which is the whole
point: a summary you cannot get behind is a summary you have to take on trust. A
digest is cached per window; `--force` regenerates it and costs a model call.

{{cmd:contact}} is one correspondent: volume in both directions, your reply times
against theirs, the typical gap, and — unless `--metrics-only` — a short briefing.
`dormant` and `declining` are the two facts worth having, and they are the
daemon's verdicts rather than a threshold this client re-derives.

{{cmd:subs}} is who sends you bulk mail and whether you read it. A row drawn as a
warning is a *candidate*: mail that keeps arriving and keeps not being read. The
`unsubscribe` column distinguishes `one click` from `link` from `by mail`, because
"there is a way out" and "there is a way out that works" are different.

{{cmd:stats ask}} answers a question about the mailbox by writing a query:

```
mail stats ask "who have I not replied to in a month"
```

The generated SQL is the first thing in the report, and that is not decoration: a
number you cannot see the query behind is a number you cannot check. `--narrate`
adds a prose summary of the rows, which is a second model call.

## Getting it out

{{cmd:export}} writes an archive. `--to` names the destination and is required —
there is no sensible default place for an interactive session to write your mail:

```
mail export 'from:stripe' --archive-format mbox -o backup.mbox
```

`--format` takes `mbox`, `maildir`, `eml` or `json`; `--with-ai` includes what the
AI passes produced. A message whose raw bytes this daemon never stored cannot be
exported, and the report says how many were skipped for that reason — an archive
quietly short by forty messages is worse than one that admits it.

A failed export leaves the partial archive on disk on purpose. Deleting a
half-written export would destroy the only copy of whatever did arrive; saying it
is incomplete is what stops it being mistaken for a whole one.

## Where to read next

- [[library]] — notes, saved searches and smart folders: the things you name and
  keep.
- [[ai-cost]] — which of these verbs spend money, and how much.
- [[halve-the-ai-bill]] — `:subs` as a worked example.
- [[reports]] — the screen these draw into, and what Enter does on a row.
