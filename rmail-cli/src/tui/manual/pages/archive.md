# Archive, move, delete

Four keys move mail, and they do genuinely different things to your server.
The difference matters because one of them is not reversible.

## Archive is a move

{{keys:message.archive}} moves the message to the account's archive folder.
There is no archive RPC and there never will be: archiving is a move, and
which folder counts as the archive is a per-server convention rather than an
operation. {{cmd:message archive}} is therefore the same call as
{{cmd:message move}} with the destination already decided for you.

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

## Delete expunges

{{keys:message.delete}} asks first, and it is the only message action that
does. {{cmd:message delete}} expunges on the server: the message leaves the
account. It does not go to a trash folder, it is not recoverable from rmail,
and the next sync will not bring it back.

If what you meant was "get this out of my way", the key you wanted is
archive. If what you meant was "I never want to see mail like this again",
the tool you wanted is a rule — see [[rule-from-mistake]].

## Where to read next

- [[undo]] — what can be taken back, and for how long.
- [[practice-triage]] — the pass these keys are meant to be used in.
