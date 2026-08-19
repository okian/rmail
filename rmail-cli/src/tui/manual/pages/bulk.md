# Acting on many messages

{{keys:visual.toggle}} starts a selection at the cursor and extends it as you
move, exactly as vim's visual mode does. Every message action then applies to
the whole selection instead of one row, and {{keys:visual.swap-ends}} jumps to
the other end so you can extend the range from the top after starting at the
bottom.

## One intent for the whole selection

A flag action over a mixed selection picks a single intent and applies it
everywhere, rather than toggling each row independently. Marking a half-read
selection read marks all of it read; it does not leave the read half unread.
The reason is that a bulk action you cannot predict the result of is a bulk
action you have to check afterwards, which costs more than doing it one row
at a time.

## The selection belongs to the list

Leaving the message list drops the range. The anchor outlives the trip — a
selection survives reading a manual page or opening a search hit — but the
range does not, so a bulk action can never act on rows that are not on the
screen you are looking at.

That is deliberate and it is the more surprising half of the rule: an archive
pressed in the viewer archives the message in the viewer, even if you had a
selection running on the list behind it.

## Wider than the screen

A selection is bounded by what you can scroll to, and some sets are not. For
those, the bulk surfaces are per-domain rather than general:
{{capability:TagBulkTag}} tags everything a filter-only query selects, and
{{capability:FinderBatchAction}} acts on a finder result set. There is no
general bulk-preview-and-undo layer, so where one of those does not fit, the
honest move is a smaller selection.

The command line takes vim's selection range, which is how a verb acts on
everything selected rather than on the row under the cursor:

```
:'<,'>message archive
:'<,'>message toggle-read
```

Opening it with {{keys:command}} while a selection is up fills the range in
already. The other two range forms — % for everything listed, and a leading
count — are part of the grammar and are refused rather than half-honoured:
nothing here can address those sets yet, and acting on one row instead would
be a range that looked obeyed and was not.

## Where to read next

- [[triage-by-selection]] — the worked example this page is the theory for.
- [[archive]] — what each of the actions does to your server.
