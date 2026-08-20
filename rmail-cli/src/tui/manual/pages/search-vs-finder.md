# Search or finder

Two different questions, two different tools, and picking the wrong one is
the most common reason rmail feels slow.

- {{keys:search}} answers "which messages are about this". It runs the full
  retrieval and ranking pipeline over message bodies, attachments, notes and
  AI summaries, and returns a ranked list. {{cmd:search}} is the same thing
  by name.
- {{keys:finder}} answers "take me to the thing I already have in mind". It
  matches short labels — subjects, folders, contacts, tags, saved searches,
  commands — as you type, against an index held in memory, and never contacts
  your mail server. {{cmd:finder}} opens it by name.

The rule of thumb: if you could name the thing, use the finder. If you could
only describe it, search.

## Saying which kind of search you mean

The search line takes free text, hard filters, or both.

```
invoice acme                     free text, ranked
from:alice has:attachment        filters, which constrain, not rank
~the contract we renegotiated    force semantic
=Q3-2026-final                   force lexical, exact terms
```

A leading tilde forces the semantic retriever, a leading equals forces the
lexical one, and with neither the pipeline runs both and fuses them. Tab
completes an operator while you type it. Unknown operators are treated as
free text rather than rejected, so a colon in a subject line is not an error.

[[practice-search]] is the one-sentence version of when to reach for each.

## Why did this match

{{keys:search.explain}} on a result expands its ranking rationale: which
retrievers surfaced it, the features that contributed most, the matched spans,
and the reranker's own one-line reason when a model reranked.
{{cmd:search explain}} is the same, addressed by name. This is the same
data an agent is handed as grounding, which is why [[grounded]] can make the
claim it does.

## Reading the plan, and the index behind it

Three verbs sit next to search rather than inside it, because each answers a
question *about* a search rather than running one.

{{cmd:search compile}} compiles a sentence into a query and shows you the plan
before anything runs — the operators it turned into, the semantic arm if there is
one, and whether the answer came from the cache or a fresh model call. A plan you
can read is a plan you can correct.

{{cmd:search entities}} searches the extracted entities rather than the messages:
the addresses, amounts, phone numbers and organisations the index pulled out.
`--kinds` narrows to some of them.

{{cmd:search eval}} scores a golden set — a file of queries with judged answers —
and reports NDCG@10, MRR, Recall@50 and P@3 per query and overall. It is the one
verb in this client that reads a file, and it has to: the RPC takes its judgments
by value so the daemon needs no access to whatever directory you are in, and a
golden set exists nowhere but on disk. A query whose judgments name messages the
index does not have is drawn as a warning, because every metric for it is then a
lower bound rather than a measurement.

```
mail search eval --golden eval/golden.toml
```

## The finder's scopes

A leading sigil narrows what is searched, and is stripped before matching:

```
  >foo   commands        #foo   tags
  @foo   contacts        /foo   saved searches
  :foo   mailboxes       foo    the current scope
```

## Where to read next

- [[saved-vs-smart]] — keeping a query rather than retyping it.
- [[index]] — what search is searching, and what it costs to keep current.
