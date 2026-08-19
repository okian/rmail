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

Accounts are addressed by id everywhere in this command line, not by name —
mail list is the quickest way to see which is which.

## What they cost to keep

A smart folder is re-evaluated on every sync, so a predicate that is
expensive to evaluate is expensive every time mail arrives. That is the
argument for expressing membership with filters wherever you can and leaving
the embedding predicate for the part that genuinely needs one.

```
mail folder eval --account 1 Unpaid       re-evaluate it now
mail folder members --account 1 Unpaid    what it currently holds
```

Smart folders can also act: a new match can be auto-tagged or raise a
notification, which is where a folder stops being a view and becomes
automation — see [[practice-rules]].

## Where to read next

- [[search-vs-finder]] — the query language these are written in.
- [[practice-tags]] — why a tag is usually the better durable label.
