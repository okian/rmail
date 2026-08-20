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
~/.local/state/rmail/rmaild.sock — and then the database beside it, $RMAIL_DB,
defaulting to ~/.local/state/rmail/rmail.db, which the daemon opens and
migrates before it binds anything.

start waits 30 seconds for the socket to answer and --timeout moves that
number; a client connecting to a socket that already exists gives up after
five seconds. A status that comes back instantly is therefore telling you
nothing is listening, not that something is slow. See [[daemon]].

## The daemon runs, but one account shows an authentication error

```
mail api call AccountService.TestConnection '{"id": 1}'
mail account refresh 1
```

An expired OAuth token refreshes on its own while the daemon is up; a machine
that has been asleep for a month may show one failure first. A password
resolved by command shows the same symptom when the command itself fails —
run the password command by hand. See [[add-oauth-account]].

A password you are certain is right is usually a provider that will not take
a password at all. iCloud, Yahoo and Fastmail want an app-specific password
generated in their own web UI, and Microsoft 365 has retired password
authentication for IMAP outright — each of those arrives here as the same
authentication failure a typo gives, because on the wire it is the same
failure. It is also the one input rmail can neither discover nor default:
[[provider-settings]] carries the row per provider and says where each one
keeps it.

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
than a guess. RUST_LOG is info when unset, and an unparsable filter falls
back to info rather than to silence; RMAIL_LOG_FORMAT is text, and anything
that is not text or json is text as well, so a typo there costs you the JSON
shape and not the logs.

## The values these checks are made against

Every symptom above is a comparison against a default. The paths and the two
log knobs are bootstrap environment variables, read before the config system
exists and not part of the RMAIL_ overlay:

```
RMAIL_SOCKET       ~/.local/state/rmail/rmaild.sock
RMAIL_DB           ~/.local/state/rmail/rmail.db
RMAIL_CONFIG       ~/.config/rmail/config.toml
RUST_LOG           info
RMAIL_LOG_FORMAT   text
```

The rest are config tables in the [[config-file]], each taking the usual
RMAIL_SYNC__INTERVAL-shaped environment override:

```toml
[sync]
interval = "5m"
poll_interval = "5m"
idle = true
```

An account's own two ports are not among them: they belong to the database
row rather than to the file — see [[add-any-account]] — and omitting them
there gives 993 and 587. That 993 is more than a convention. Whatever port
you set, the IMAP connection is TLS
from the first byte — there is no STARTTLS on that side and no plaintext at
all — so a server offering only 143 cannot be reached by changing the number.
The two five-minute intervals are why late mail is not a symptom until five
minutes have gone: with idle true the server usually pushes first, and the
interval is the ceiling on how long a missed push stays invisible.

Three deadlines decide how long a symptom takes to appear, and none of them is
a config key. Connecting to the socket gives up after five seconds. Every IMAP
round trip is capped at 30 seconds, so a move that fails at 30 seconds failed
at the server. This client wraps every unary call at 120 seconds and says so —
timed out after 120s. And mail --deadline sends a gRPC deadline of whatever
length you ask for, with no default: pass nothing and nothing is sent, so the
daemon works until it is finished rather than until you stopped waiting.

One check here has no default because it has no setting. The trust store is
the public root list compiled into the binary; there is no per-account CA
field, and writing one into the config file is refused as an unknown key. A
server presenting a certificate signed by your own root is unreachable with
the hostname, the port and the password all correct, and the only thing that
tells you so is that the error names the TLS handshake rather than the login.
