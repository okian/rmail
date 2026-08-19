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
~the contract we renegotiated in spring     what it was about
```

Filters constrain rather than rank, so adding one never pushes the answer
further down the list — it removes everything that could not have been it.
That makes a filter the cheapest thing you can add to a query and the first
thing to reach for.

## And when the answer is a name

If you could name the thing rather than describe it, you wanted the finder,
not search. [[search-vs-finder]] draws that line.
