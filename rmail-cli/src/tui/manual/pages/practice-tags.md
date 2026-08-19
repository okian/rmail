# Practice: tag for retrieval

Add a tag when it is how you will later look for the message, not to record
what the message is. The folder it arrived in already records that.

## Why

A tag you would never type into a search is a tag that costs maintenance and
returns nothing.

## What that rules out

- Tagging everything from one sender. You can search for the sender.
- Tagging by date, category or read state. All three are already operators.
- A hierarchy deeper than two levels. If you cannot remember the middle
  segment you will not find the tag, and tag:project/* exists precisely so
  the middle can be skipped.

## What it rules in

Cross-cutting labels no header carries: which client a thread belongs to,
whether an invoice is paid, which release a bug report is about. These are
worth typing because nothing else can produce them.

{{capability:TagSuggestTags}} will propose tags for a message, and
{{capability:TagResolveSuggestion}} is where you accept or reject them —
worth using precisely because rejecting a suggestion is the cheap way to find
out that a tag was not one you would ever have searched for.

## They leave the machine

A tag round-trips as an IMAP keyword, so it lands on your mail server with a
prefix. That is a feature — other clients see it — and it is also the reason a
tag is not a private note. Use [[practice-notes]] for that.
