# Practice: say which kind of search you mean

Lead with a filter when you know a fact about the message, with an equals
when you remember the exact words, and with a tilde when you can only
describe it.

## Why

The three retrievers are good at different questions, and an unqualified query
makes the pipeline guess which one you are asking.

## The three shapes

```
from:alice has:attachment after:last-week   facts you are sure of
=Q3-2026-final                              words you remember exactly
~"the contract we renegotiated in spring"   what it was about
```

Filters constrain rather than rank, so adding one never pushes the answer
further down the list — it removes everything that could not have been it.
That makes a filter the cheapest thing you can add to a query and the first
thing to reach for.

## Where the mode comes from

A query with no sigil on it runs in the mode the daemon is configured for:
search.default_mode in the [[config-file]]'s search table, which ships as
hybrid, so every enabled retriever runs and their lists are fused by rank.
Typing neither sigil is therefore not the absence of a choice — it is the
hybrid one, and the two sigils are how you overrule it for one word without
touching the file.

A sigil is narrower than that setting in two ways. It binds to the single
whitespace-delimited token it precedes, which is why the tilde line above
quotes its words: without the quotes it would force semantic retrieval for
the word "the" and leave the other five words ranked hybrid. And it applies
to free text only, because a hard filter is not ranked and so has no ranking
for a sigil to change — ~tag:work is a search by meaning for the characters
tag:work, not a semantic tag filter. A lone ~ or = with nothing after it is
the literal character.

Which retrievers actually surfaced the result you are looking at is a
question for {{keys:search.explain}} rather than for the config file, and
mail search --mode, which takes lexical, semantic or hybrid, is the way to
move a whole query off default_mode for one run.

## And when the answer is a name

If you could name the thing rather than describe it, you wanted the finder,
not search. [[search-vs-finder]] draws that line.
