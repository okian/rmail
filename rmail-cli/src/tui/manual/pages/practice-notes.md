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
they are part of what search retrieves over. That is the payoff for writing
them in sentences rather than in keywords.

## On a thread, not just a message

{{capability:NoteListNotes}} reads notes for either. Attaching the decision to
the thread is usually right, because the thing you will look for later is the
conversation and not the message that happened to be open when you decided.
