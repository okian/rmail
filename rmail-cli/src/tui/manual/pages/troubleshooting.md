# Troubleshooting

Symptoms first, in the order they are usually seen. The first question is
always which of three things is unavailable: the daemon, the network, or a
provider.

## Nothing loads, and the status line says the daemon is not running

```
mail daemon status
mail daemon start
```

Every verb except that one refuses with FAILED_PRECONDITION and names it,
rather than starting a daemon you did not ask for. If start fails, the socket
path is the next thing to check — $RMAIL_SOCKET, defaulting to
~/.local/state/rmail/rmaild.sock. See [[daemon]].

## The daemon runs, but one account shows an authentication error

```
mail api call AccountService.TestConnection '{"id": 1}'
mail account refresh 1
```

An expired OAuth token refreshes on its own while the daemon is up; a machine
that has been asleep for a month may show one failure first. A password
resolved by command shows the same symptom when the command itself fails —
run the password command by hand. See [[add-oauth-account]].

The rest of the client keeps working throughout: one account failing to log
in does not take the local database with it.

## Mail is there, but search does not find it

```
mail index status
mail index verify
```

Lexical coverage complete and semantic incomplete means new mail is findable
by word and not yet by description. That is a queue with work left in it, not
damage. Draining resumes where it stopped. See [[practice-index]] and
[[recover-interrupted-rebuild]].

## Search finds it, and the ranking is wrong

{{keys:search.explain}} on the result says which retrievers surfaced it and
what contributed. If a reranker is configured to reach a model and cannot,
the search degrades to the local ranking rather than failing — so a quieter
ranking is a symptom of an unreachable provider, not of a broken index. See
[[search-vs-finder]].

## An AI feature says it is unavailable

In order: is AI enabled at all, is the account opted out, is a policy rule
marking the folder forbidden or local-only, and is the budget spent.

```
mail ai status
mail ai budget status
```

A spent soft cap downgrades the model rather than blocking, so "it got worse"
and "it stopped" are different diagnoses. See [[ai-cost]] and [[privacy]].

## A message was flagged for prompt injection

The AI-decided action is withheld until a human confirms it. That is the shield
working: mail is attacker-controlled text, and a message containing an
instruction override or hidden text gets no unattended action. Confirm it
deliberately or leave it. See [[privacy]].

## A scheduled send has not gone out

```
mail outbox
mail outbox show <id>
```

Failed rows carry their last error. Being offline does not mark a send failed
— it stays scheduled with a next attempt time and is retried with backoff,
and nothing is dropped for being late. See [[undo]] and [[offline]].

## A webhook produced nothing

```
mail webhook deliveries --destination <name>
```

Distinguish never generated from generated and not delivered before anything
else. A delivery whose attempts are spent needs a replay. See
[[digest-to-slack]].

## A key does the wrong thing, or nothing

Check which layer you were standing in — [[modes]] for the table, [[keys]]
for what is bound where. A binding added to the wrong layer is the usual
cause. A keys.toml that stopped parsing keeps the previous bindings and
reports on the status line rather than clearing them. See [[keys-toml]].

## Logs

```
RUST_LOG=debug mail daemon start
RMAIL_LOG_FORMAT=json
```

Everything is structured tracing with per-request spans carrying the account
and mailbox, so grepping for one account's failures is a field match rather
than a guess.
