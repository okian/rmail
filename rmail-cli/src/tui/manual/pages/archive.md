# Archive, move, delete

Four keys move mail, and they do genuinely different things to your server.
The difference matters because one of them is not reversible.

## Archive is a move

{{keys:message.archive}} moves the message to the account's archive folder.
There is no archive RPC and there never will be: archiving is a move, and
which folder counts as the archive is a per-server convention rather than an
operation. {{cmd:message archive}} is therefore the same call as
{{cmd:message move}} with the destination already decided for you.

Which folder that is, is not configuration: rmail takes the first of
Archive, Archives and All Mail that the account actually has, matched on
the folder's last path segment and ignoring case, so a nested special
folder — an Archive under INBOX, or Gmail's All Mail — is found rather
than missed. The folder you are looking at is never the destination, and
an account with none of the three names refuses the key with "no archive
folder on this account" rather than picking something for you.

That also means archiving is undone by moving the message back. Nothing is
lost, and the message keeps its flags, its tags and its place in its thread.

## Move and copy

- {{keys:message.move}} opens a folder picker and moves. The message leaves
  the folder you are looking at.
- {{keys:message.copy}} opens the same picker and copies, which is
  {{cmd:message copy}} by name. The source folder is unchanged, and the copy
  appears in the destination when that folder next syncs — which is why the
  list does not visibly change when you press it.

Both act on the visual selection when there is one — see [[bulk]].

Every action on this page stops at a hundred messages at a time. A larger
selection is refused, with the count, rather than truncated to the first
hundred: you would have no way of knowing which hundred moved. A selection
cannot run past the 500 rows the list has loaded in any case, so the
hundred is the limit you meet first.

## Delete expunges

{{keys:message.delete}} asks first, and it is the only message action that
does. {{cmd:message delete}} expunges on the server: the message leaves the
account. It does not go to a trash folder, it is not recoverable from rmail,
and the next sync will not bring it back.

The prompt answers to y or Y; n, N, q and Escape back out, and any other
key leaves it standing. It is not a setting you can turn off — a trailing
bang on the command line answers the question as it opens, so
message delete! is the deliberate way past the prompt and the only one.

If what you meant was "get this out of my way", the key you wanted is
archive. If what you meant was "I never want to see mail like this again",
the tool you wanted is a rule — see [[rule-from-mistake]].

## Where the destinations are set

Nothing on this page is a setting: the three archive names and the
hundred-message cap are constants in the TUI rather than keys you can
edit. What is configuration is the destination used by the two things
that archive without you — the rules engine and the inbox agent — because
neither has a folder list in front of it:

```toml
[rules]
archive_mailbox = "Archive"

[agent]
archive_mailbox = "Archive"
```

Those are the shipped values, they live in the [[config-file]], and each
takes the usual override — RMAIL_RULES__ARCHIVE_MAILBOX,
RMAIL_AGENT__ARCHIVE_MAILBOX. Each names one mailbox by its full name and
matches it exactly, with no fallback list behind it: on an account whose
folder is called Archives, or is nested as INBOX/Archive, a rule that
archives fails with "this account has no mailbox named Archive" until you
set the key. Archiving by hand has the three names to fall back on;
archiving by rule has the one name you gave it. The agent's copy of the
key does nothing at all until agent.allow_mutations is on, and that ships
off.

## Where to read next

- [[undo]] — what can be taken back, and for how long.
- [[practice-triage]] — the pass these keys are meant to be used in.
