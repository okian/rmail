# Grounded answers

{{keys:ask}} asks a question about your mailbox and streams back an answer
with citations. The word grounded is doing real work in that sentence, and
this page is what it means.

## The daemon decides, not the model

An answer is assembled by retrieving messages first and writing from them
second. Whether the result was actually supported by what was retrieved is
judged by the daemon against the citations, not asserted by the model about
itself. A model that vouches for its own grounding is a model with an
incentive and no evidence.

So an answer arrives in one of three states: grounded in cited mail,
partially grounded with the unsupported parts marked, or ungrounded — and the
third is reported rather than hidden, because an ungrounded answer that looks
like a grounded one is worse than no answer.

## Citations are addresses, not footnotes

Every citation names a message you can open. That is the same data
{{keys:search.explain}} shows for a search result, and it is the point of
{{cmd:ask}} rather than pasting mail into a chat window: the answer stays
attached to the mail it came from, so checking it is one keypress rather than
a second search.

## What it costs

{{capability:AiAskMailbox}} is a model call, every time, and it is priced as
one — see [[ai-cost]]. Retrieval is bounded before the model is reached: a
fixed number of messages, a token ceiling on the assembled context, and a
per-message character cap so one enormous message cannot crowd out every
other citation.

## What it will not read

Mail in a folder your AI policy marks forbidden is invisible to this, and
mail flagged by the injection shield is handled as data rather than as
instructions. [[privacy]] is the whole of that story.

## Where to read next

- [[find-the-clause]] — a worked example of asking rather than searching.
- [[ai-cost]] — the budget this spends from.
