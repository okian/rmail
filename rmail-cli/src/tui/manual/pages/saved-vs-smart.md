# Saved searches and smart folders

Both are a query you did not want to retype. They differ in when the query
runs, and that difference is the whole reason there are two of them.

- A saved search is a named query string. It runs through the full pipeline
  when you ask for it, and gives you the ranked answer as of now.
- A smart folder is a saved query re-evaluated on every sync, so its
  membership is live. It looks like a mailbox and behaves like one, and no
  mail is moved on your server to make that true.

A saved search is a question. A smart folder is a standing answer.

## Defining one in plain English

```
mail folder new --account 1 "invoices I have not paid"
mail folder new --account 1 --name Unpaid 'tag:invoice -tag:paid'
```

The first has a model compile the sentence once into a stored hybrid plan of
operators, full-text terms and an embedding predicate; that compilation is
paid for once rather than on every evaluation.
{{capability:SavedSearchCompileSmartFolder}} is the step being paid for, and
it is the only part of a smart folder that ever reaches a model. Add
--predicate and the same positional is read as an operator expression
instead, which reaches nothing.

The sentence is capped at 500 characters, and a longer one is refused before
the model call rather than after it. Compiles are cached per account by the
sentence, so defining the same folder twice costs one call and --refresh is
how you ask for a second. Leave --name off and the name is a slug of the
description — its first six words, lowercased, stripped to alphanumerics and
joined with hyphens, so the first line above defines a folder called
invoices-i-have-not-paid. A name is at most 128 characters, unique per
account and matched case-insensitively, and the predicate the compile
produces is bounded at 4096 bytes; those are a saved search's own two bounds,
because both go through the same validator.

A folder with an embedding arm also carries the cosine floor a candidate has
to clear to enter through that arm. It is 0.6, no request field sets it to
anything else, and it is stamped into the row when the folder is created
rather than read at evaluation — so revising that number in a later build
cannot silently redefine what a folder you already trust contains.

Accounts are addressed by id everywhere in this command line, not by name —
mail list --all is the quickest way to see which is which, and
mail folder list --account 1 is how you read back the folders you defined.

## What they cost to keep

A smart folder is re-evaluated on every sync, so a predicate that is
expensive to evaluate is expensive every time mail arrives. That is the
argument for expressing membership with filters wherever you can and leaving
the embedding predicate for the part that genuinely needs one.

```
mail folder eval --account 1 Unpaid       re-evaluate it now
mail folder members --account 1 Unpaid    what it currently holds
```

The background pass wakes every five seconds and re-evaluates only the
accounts that saw an event; those five seconds are a compiled-in constant,
not a config key, and they bound only how quickly an auto-tag or a
notification follows the sync that caused it. Reading membership never waits
for that pass, because members recomputes the predicate against the local
database at the moment you ask. Its --limit is 0 by default, which streams
every member rather than a first page.

Two caps are worth knowing before you read a count as the whole truth. The
embedding arm contributes at most 500 messages, because a nearest-neighbour
index answers "the nearest k" and membership has no next page to reach the
rest with — so a hybrid folder is bounded in a way a deterministic one is
not, and a message can leave one because other mail moved closer to the
query, not because anything about it changed. And eval's entered and departed
lists stop at 256 ids each while the counts beside them stay exact, a delta
being unbounded in principle; re-derive the full set with members rather than
reading the sample as the answer.

Smart folders can also act: a new match can be auto-tagged or raise a
notification, which is where a folder stops being a view and becomes
automation — see [[practice-rules]].

## Where the numbers come from

Neither of these is configuration. A saved search and a smart folder are each
a row in the local database, made and unmade by verbs, and the
[[config-file]] has no table for either — there is nothing to set before you
define one, and nothing to read there afterwards.

For a smart folder those verbs are the five under mail folder: new, list,
members, eval and rm. A saved search has no verb of its own. Its RPCs are
served, tested and reachable — {{capability:SavedSearchCreateSavedSearch}}
makes one, {{capability:SavedSearchListSavedSearches}} lists them,
{{capability:SavedSearchRunSavedSearch}} runs one — what nobody has
written is the command-line surface over them, so the generic client is how
you reach them, and it is the same call an agent makes over MCP:

```
mail api call SavedSearchService.CreateSavedSearch \
  '{"account_id": 1, "name": "Unpaid", "query": "tag:invoice -tag:paid"}'
```

One configured value does reach in. A run that names no limit of its own uses
the daemon's search.default_limit, 25 out of the box in the config file's
search table and overridable as RMAIL_SEARCH__DEFAULT_LIMIT, and it is ranked
under search.default_mode, hybrid, with no per-run override for it the way a
typed search has --mode.

Reaching a saved search by name is the finder's job rather than a verb's:
{{keys:finder}} in the TUI, or mail find /unpaid from a shell, where the
leading slash restricts the search to saved searches.

## Where to read next

- [[search-vs-finder]] — the query language these are written in.
- [[practice-tags]] — why a tag is usually the better durable label.
