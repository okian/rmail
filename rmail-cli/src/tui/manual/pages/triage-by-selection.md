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

## Deal with the ones that need a reply, later

Do not answer anything during triage. Flag it, finish the pass, and come back
to a list of five rather than two hundred. When you do reply, arm the
follow-up at the same time — [[practice-followups]] gives the reason.

## What you should not have done

Nothing in this pass deleted anything. Archive is a move and is reversible;
delete expunges and is not. If a category of mail comes back tomorrow and you
archive it again, that is the moment to write a rule instead — see
[[rule-from-mistake]].
