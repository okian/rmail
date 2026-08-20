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

The verdict is one bit, and it is mechanical: an answer is grounded when at
least one bracketed label in its prose resolves to a message this daemon
retrieved. When none does, the answer arrives ungrounded and carries the
reason — either retrieval found nothing usable, in which case no model was
called at all, or the model answered and cited nothing real. There is no
partially-grounded middle state, and the ungrounded one is reported rather
than hidden, because an ungrounded answer that looks like a grounded one is
worse than no answer.

## Citations are addresses, not footnotes

Every citation names a message you can open. That is the same data
{{keys:search.explain}} shows for a search result, and it is the point of
{{cmd:ask}} rather than pasting mail into a chat window: the answer stays
attached to the mail it came from, so checking it is one keypress rather than
a second search.

## What it costs

{{capability:AiAskMailbox}} is a model call, every time, and it is priced as
one — see [[ai-cost]]. Retrieval is bounded before the model is reached: 12
messages retrieved, 8,000 estimated tokens of assembled context, and 2,000
characters from any one message, so a single enormous message cannot crowd
out every other citation. Packing stops at the first message that would cross
the token ceiling rather than skipping past it to a shorter, less relevant
one. The answer itself is capped at 1,024 output tokens, which is a bound
rather than a target — the prompt asks for a few sentences and no preamble.

## What it will not read

Mail in a folder your AI policy marks forbidden is invisible to this, and
mail flagged by the injection shield is handled as data rather than as
instructions. [[privacy]] is the whole of that story.

## The bounds on an answer, and where they live

All five sit in the [[config-file]]'s ai.ask table, and a fresh install runs
on these:

```toml
[ai.ask]
model = "claude-sonnet-5"
top_k = 12
max_context_tokens = 8000
max_chars_per_message = 2000
max_tokens = 1024
```

model is the sonnet tier, not the opus one ai.models.deep uses for a
per-message deep pass: retrieval-augmented answering runs on the balanced
default rather than the expensive one. It is also not necessarily the model
that answered — a soft budget cap downgrades the model before it blocks
anything — so the retrieval trace names the one actually called, beside how
many messages were retrieved and how many were packed, and, when either is
not zero, how many the AI policy withheld and how many the context budget
dropped. The ask pane shows that line while the answer streams, and mail ask
prints it with --trace.

max_chars_per_message is clamped by ai.privacy.max_body_chars, 40,000 out of
the box, and the smaller of the two wins — a per-answer ceiling cannot raise
the operator's own limit on how much of a body may leave the machine. Every
field takes an environment override with the usual shape,
RMAIL_AI__ASK__TOP_K or RMAIL_AI__ASK__MAX_CONTEXT_TOKENS. For one question
only, mail ask --top-k stands in for top_k — higher or lower — and leaves
the file alone.

## Where to read next

- [[find-the-clause]] — a worked example of asking rather than searching.
- [[ai-cost]] — the budget this spends from.
