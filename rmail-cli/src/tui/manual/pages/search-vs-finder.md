# Search or finder

Two different questions, two different tools, and picking the wrong one is
the most common reason rmail feels slow.

- {{keys:search}} answers "which messages are about this". It runs the full
  retrieval and ranking pipeline over message bodies, attachments, notes and
  AI summaries, and returns a ranked list, 25 results deep by default.
  {{cmd:search}} is the same thing by name.
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

With no sigil the current scope stands, and out of the box that scope is
every kind at once: finder.default_scope is all. A sigil overrides it for the
keystrokes it precedes rather than changing it. Matching is smart-case, so a
lower-case query ignores case and a single upper-case character anywhere in it
makes the whole query case-sensitive.

## The two tables behind this page

Search and the finder are configured separately. The fields that decide what
either one does live in the [[config-file]]'s search table and its finder
table, and a fresh install runs on these:

```toml
[search]
default_mode = "hybrid"
fusion = "rrf"
rrf_k = 60
candidates_per_source = 200
top_k_rerank = 50
default_limit = 25
rerank = "auto"
learning = true

[finder]
enabled = true
default_scope = "all"
max_results = 200
max_entries = 200000
max_memory_mb = 25
max_drain_batch = 2000
refresh_interval_ms = 250
smart_case = true
preview = true
```

The search numbers are one funnel, and reading them in order describes what a
query does. default_mode is hybrid, so every enabled candidate source runs and
each returns up to 200; fusion merges those lists by rank rather than by score,
which is why a message several sources found outranks one that a single source
found strongly; the first-stage ranker keeps the top 50 of the fused pool for
the reranker; and default_limit hands you 25 of those. rrf_k, at 60, is what
damps how far rank 1 of a list outweighs rank 10 of it.

rerank is auto — the local cross-encoder on an interactive search, Claude on a
deep one. Reranking is best-effort in every mode: a missing local model, a
provider failure or an exhausted AI budget leaves the first-stage ranking in
place rather than failing the search, so what actually ran on a given query is
a question for {{keys:search.explain}} and not for the file.

learning is true, and what it collects stays on this machine. The
implicit-feedback log behind it — which results a search showed you, and which
of them you opened — is written to your own database, and no RPC reads it back
out. Setting it false stops those rows being written at all, rather than
written and then ignored. See [[privacy]].

The finder's two size bounds are why it can fail to find something that
exists, so they are worth knowing rather than looking up. The in-memory
index holds at most 200000 entries, or 25 MiB of them, whichever binds first —
about 100k messages — and entries load newest-first, so what a full store
turns away is your oldest mail. {{cmd:finder status}} reports how many entries
are resident, how much heap they hold, how many feed rows are still waiting,
and how many a cap refused; a non-zero refusal count means the index is
deliberately incomplete. max_results is a bound on one answer rather than on
the index.

The last two finder numbers are freshness. The change feed is drained into the
index every 250 milliseconds, 2000 rows a pass, so a resync that rewrites a
whole mailbox costs the finder a few seconds of staleness rather than stalling
the writer behind it.

Every field takes an environment override with the same shape,
RMAIL_SEARCH__DEFAULT_MODE or RMAIL_FINDER__MAX_MEMORY_MB, so a one-off does
not need an edit.

## Where to read next

- [[saved-vs-smart]] — keeping a query rather than retyping it.
- [[index]] — what search is searching, and what it costs to keep current.
