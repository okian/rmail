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

Rejecting is also recorded, and not only for your benefit. Once a tag has at
least three of your decisions inside the last ninety days, one you have
rejected three times out of four stops being suggested at all, and one
rejected less often than that is held to a proportionally higher confidence
before it may ever apply itself. Neither is permanent: the decisions age out
of the ninety-day window and the tag is offered again.

## They leave the machine

A tag round-trips as an IMAP keyword, so it lands on your mail server with a
prefix. That is a feature — other clients see it — and it is also the reason a
tag is not a private note. Use [[practice-notes]] for that.

The prefix is rmail/ out of the box, so a tag named client/acme reaches the
server as the keyword rmail/client/acme, and the slash inside that name is
the same separator tag:project/* searches on. Both are fields of the tags
table in the [[config-file]] — tags.imap.keyword_prefix and
tags.hierarchy_separator — as is tags.default_sync_mode, which ships as auto:
a tag attempts the keyword, and on the first server refusal it is written
back as sync_mode local and never attempts the wire again, with the tag still
applied locally. Set that field to imap to have the refusal surface as an
error instead, or to local to keep tags off the wire entirely. Every field
there also takes an environment override with the same shape,
RMAIL_TAGS__DEFAULT_SYNC_MODE or RMAIL_TAGS__IMAP__KEYWORD_PREFIX.

## Where the auto-apply numbers come from

Nothing the classifier suggests is applied for you unless an enabled tag rule
names that tag with mode auto, so a mailbox with no rules never has a tag put
on it without being asked. {{capability:TagListTagRules}} is what mail
tag-rules list reads — an account's rules, the retired ones included — and
{{capability:TagSetTagRule}} is what mail tag-rules set writes. That command
defaults --mode to suggest, which authorizes nothing, and --min-conf to 0.9.
A rule's own floor can only be stricter than the mailbox-wide
tags.ai.auto_apply_min_confidence, which is 0.85 — the effective floor is the
higher of the two, raised further by the rejection record above.

The rest of the pass is the tags table's ai section: three suggestions per
message at most (tags.ai.max_suggestions), drawn only from the eight names in
tags.ai.taxonomy — work, personal, finance/invoice, finance/receipt, travel,
newsletter, urgent and follow-up — and run on newly synced mail because
tags.ai.suggest_on_new_mail ships true. A name outside that list can never
become a tag, which is why the taxonomy is a config field and not something
the model is allowed to extend.
