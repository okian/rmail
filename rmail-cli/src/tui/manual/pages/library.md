# Notes, saved searches and smart folders

Three things you name and keep. A **note** is what you wrote about a message. A
**saved search** is a query stored under a name. A **smart folder** is a
predicate with membership — messages enter and leave it, and it can act on what
enters.

## Notes

{{cmd:note list}} is the notes on the message you have open; `--thread` shows the
thread's instead. A note written by the AI is drawn dim, and that distinction is
deliberate: a summary the model wrote and a decision you recorded are different
claims, and a listing that drew them identically would invite you to treat one as
the other.

{{cmd:note add}} writes one:

```
mail note add 42 --message "chased this on Tuesday"
```

{{cmd:note edit}} takes the id and the new text; {{cmd:note rm}} takes the id.
{{cmd:note watch}} is the same listing, live — it appends as notes are added,
edited and deleted, and a deletion arrives as a row saying so rather than by
quietly rewriting what you are looking at.

## Saved searches

{{cmd:saved list}} is what you have stored. Enter on a row runs it.

{{cmd:saved save}} stores a query under a name, and {{cmd:saved edit}} rewrites
one. Two verbs rather than an upsert, because `Create` refuses a name that exists
and `Update` refuses one that does not — and an upsert would quietly store a
typo'd name as a new entry:

```
mail api call SavedSearchService.CreateSavedSearch '{"account_id":1,"name":"unpaid","query":"from:stripe is:unread"}'
```

{{cmd:saved run}} runs one and streams the hits in rank order; `--explain` asks
for each hit's ranking explanation. {{cmd:saved rm}} forgets it.

`SavedSearchService` has no `mail` verb at all, so these spellings are the first
ones anywhere — a future `mail saved` will have to adopt them.

## Smart folders

A smart folder is not a saved search with a different name. Running a search
searches, now, and gives you hits; a smart folder has *membership*, so messages
enter and depart it, and it can tag or notify on what enters. [[saved-vs-smart]]
is the long version.

{{cmd:folder list}} is what exists. A folder with an auto-tag is drawn as a
warning, because it changes mail on its own. Enter on a row lists what is in it,
which is {{cmd:folder members}}.

{{cmd:folder new}} takes a predicate written in the query operators.
{{cmd:folder compile}} takes a *sentence* and has a model compile it once into a
stored plan:

```
mail folder new unpaid --account 1 --predicate 'from:stripe is:unread'
```

Two verbs, not one flag, because one of them spends money at a provider and the
other does not — and that is not a difference to hide behind whether a flag was
given. `--auto-tag` applies a tag to whatever enters; `--notify` pings on it.

{{cmd:folder eval}} re-evaluates membership and reports what entered, what
departed, and how many messages were tagged as a result. That last row is the one
that says your mail was changed. {{cmd:folder rm}} forgets the folder.

## Where to read next

- [[saved-vs-smart]] — the distinction, at length.
- [[content]] — the rest of what is in the mail.
- [[search-vs-finder]] — which search these queries go through.
- [[reports]] — the screen these draw into, and what Enter does on a row.
