# Practice: write down why

Attach a note when you have decided something the message does not say — the
reason, not a restatement of the contents.

## Why

The message already contains what it says; what disappears in three months is
why you did what you did about it.

## The shape of a good note

- "Agreed to the extension on the call, not in writing anywhere."
- "Superseded by the thread from the 14th."
- "Do not reply — legal is handling it."

And the shape of a useless one: "invoice from Acme". That is the subject
line, and a search would have found it.

## Notes are local and private

{{capability:NoteAddNote}} stores a note in the local database. It does not
round-trip to your mail server, which is exactly the difference between a note
and a tag: a tag is a label you will search for and that lands on the server,
a note is a sentence for you. See [[practice-tags]].

Notes are indexed, so they are searchable by has:note and by their text, and
they are part of what search retrieves over. Their text carries weight 3.0
in the field-weighted lexical index — three times what a body's 1.0 counts
for, under a subject's 8.0 — so a phrase matched in a note you wrote is
worth three of the same phrase quoted in a reply. That weight is
search.bm25_weights.notes in the [[config-file]], and it is the payoff for
writing notes in sentences rather than in keywords.

## On a thread, not just a message

{{capability:NoteListNotes}} reads notes for either. Attaching the decision to
the thread is usually right, because the thing you will look for later is the
conversation and not the message that happened to be open when you decided.

## Where a note lives, and what limits it

A note is a row in the notes table of the local database — $RMAIL_DB, which
defaults to $HOME/.local/state/rmail/rmail.db — and not a field of the
message. Deleting the message or the thread it hangs on takes the note with
it, by a cascade in the schema rather than a cleanup pass, so it goes at the
same moment.

Nothing bounds the text: there is no length limit, and no cap on how many
notes one message or thread carries. The single rule is that a body which is
empty after trimming is refused, so mail note add on an editor buffer you
left untouched aborts without writing anything.

The notes table in the [[config-file]] holds three fields, and notes.index
is the one worth knowing rather than looking up: true out of the box, and
set false it leaves notes stored, listed and watched exactly as before while
they stop feeding the lexical index. note: and has:note keep working even
then, because both compile to a query against the notes table rather than
against the index — only the free-text half goes quiet. The other two are
declared and not yet wired: notes.preview_lines is 6 and notes.editor is
$EDITOR, and nothing reads either one — no surface renders a note preview,
and mail note add with no -m resolves the EDITOR variable itself, falling
back to vi when it is unset, without consulting the config at all. Each
field takes an environment override with the usual shape,
RMAIL_NOTES__INDEX or RMAIL_NOTES__PREVIEW_LINES.
