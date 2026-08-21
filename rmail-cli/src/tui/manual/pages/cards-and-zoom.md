# Cards, focus and zoom

The screen is being rebuilt around four cards — sidebar, list, reader and
rail — that can each hold the keyboard's focus, and each be zoomed to fill
the frame. Three keys already drive that model: {{keys:card.zoom}},
{{keys:sidebar.toggle}} and {{keys:rail.toggle}}. The screen this build
draws is still the three panes [[tour]] describes, and none of them are
one of the four cards yet, so do not expect a keystroke here to rearrange
what you are looking at the way it eventually will. What you get today is
smaller and already real: a status line that says what changed, and, for
`Z`, a full-frame placeholder naming the card it zoomed — proof the state
underneath is live, not a promise about what the eventual card will show
there.

## What each key changes

- {{keys:card.zoom}} zooms the focused card, replacing the whole screen with
  a block that names it, or unzooms it if that card is the one already
  zoomed. It only reaches from the three-pane screen — pressed with a
  message open, it says so and changes nothing rather than zooming a card
  behind a screen that cannot draw one. Zoom names at most one card at a
  time: it is not "the reader is zoomed" and separately "the list is
  zoomed," it is one slot that says which card, if any. `Z` always targets
  whichever card currently has focus, never "whichever was zoomed last" —
  zoom the reader, move focus to the list, press `Z` again, and it is the
  list that is now zoomed, not the reader a second time.
- {{keys:sidebar.toggle}} and {{keys:rail.toggle}} each have two jobs
  depending on how wide the terminal is. At 120 columns or more, the key
  flips whether that card defaults to visible — a preference the eventual
  renderer will consult once it draws the four cards at all. Narrower than
  that, the preference has nothing to attach to, so the same key instead
  moves focus to the card, which is how it still finds a way onto screen at
  a width that would otherwise have hidden it. Same key, same meaning
  either width: "show me this."
- Moving focus this second way also clears any zoom. Asking to see the
  sidebar or the rail takes priority over whatever was zoomed before —
  otherwise the zoomed card would keep the screen (zoom is checked first)
  while focus quietly pointed at a card nothing was showing.

## Why one key does two things

A key that only toggled visibility would do nothing useful below 120
columns, and a key that only moved focus would leave wide terminals with no
fast way to hide a card once it was shown. Neither half is the odd one out;
both are "make this card visible," and which of the two mechanisms answers
that is a property of the terminal, not something to remember separately
per width.

## Where to read next

- [[tour]] — the screen these three keys do not rearrange yet.
