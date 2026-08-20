# Worked example: triage by selection

Two hundred unread messages in the inbox, most of them noise. The goal is to
be through them in one pass, without opening anything you do not have to.

## Sort the screen before you touch it

Turn the AI panel on with {{keys:ai.panel}} and walk down the list. The panel
is reading the triage pass that already ran at sync time, so moving the cursor
costs nothing — you are reading a summary, a category and a priority that were
written when the mail arrived. See [[ai-cost]].

## Take out the block that is obviously noise

Newsletters and receipts tend to arrive in runs. Put the cursor on the first
one, press {{keys:visual.toggle}}, move down to the last, and archive the whole
range in one keystroke. A mixed selection gets one intent applied to all of
it, so there is nothing to check afterwards — see [[bulk]].

One action acts on at most a hundred messages, so a run longer than that
is two ranges rather than one, and two hundred unread never goes in a
single keystroke. Past the hundred the action is refused, with the count
you selected next to the hundred it will take, rather than truncated to the
first hundred — you would have no way of knowing which hundred moved. The
list loads 500 rows of a folder and does not page past them, so the hundred
is the bound you meet first, and neither number is a setting: both are
compiled in.

Once the range is right, {{keys:command}} opens with '<,'> already typed, so
the whole selection is one line away:

```
:'<,'>message archive
```

## Mark what is left

Of what remains, most needs one of two labels: read-and-done, or
needs-a-reply. {{keys:message.toggle-read}} and {{keys:message.toggle-flag}}
are those two, and doing them from the list rather than the viewer is the
whole point of the pass.

Neither is an rmail-side annotation: they set the IMAP flags Seen and
Flagged on the server itself, so the pass is visible from every other client
reading the account rather than only here. Over a selection each key decides
its direction once — the flag is cleared only when every message in the range
already carries it, and set otherwise — so marking a half-read run read marks
all of it read. The hundred-message cap applies to these two as well, with
the same refusal.

## Deal with the ones that need a reply, later

Do not answer anything during triage. Flag it, finish the pass, and come back
to a list of five rather than two hundred. When you do reply, arm the
follow-up at the same time — [[practice-followups]] gives the reason.

## What you should not have done

Nothing in this pass deleted anything. Archive is a move and is reversible;
delete expunges and is not. If a category of mail comes back tomorrow and you
archive it again, that is the moment to write a rule instead — see
[[rule-from-mistake]].

## Where the archive landed, and what you cannot tune

None of the moves and flags in this pass read a setting. Archiving by hand
takes the first of Archive, Archives and All Mail that the account actually
has, matched on the folder's last path segment and ignoring case, so a nested
Archive under INBOX, or Gmail's All Mail, is found rather than missed. The
folder you are looking at is never the destination, and an account with none of
the three names refuses the key with "no archive folder on this account"
instead of choosing something for you — see [[archive]]. Neither those three
names nor the hundred-message cap has a config table or an environment
override behind it, because a cap you can raise from a file stops bounding
what one keystroke may cost.

What is configuration is where the rule you write afterwards archives to,
because the evaluator has no folder list in front of it:

```toml
[rules]
archive_mailbox = "Archive"
```

That is the shipped value, it lives in the [[config-file]], and it takes the
usual override, RMAIL_RULES__ARCHIVE_MAILBOX. It names one mailbox by its
full name and matches it exactly, with no fallback list behind it: on the
account whose folder is called Archives, the key that worked by hand fails by
rule until you set it.
