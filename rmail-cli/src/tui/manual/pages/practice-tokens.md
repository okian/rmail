# Practice: the narrowest token that works

Mint a token per consumer, scoped to what that consumer does, and revoke it
when the consumer goes away.

## Why

A token is the only thing standing between an agent and your whole mailbox on
any path that is not the local socket.

## The three commands

```
mail token create --name claude --scope mail.read
mail token list
mail token revoke <id>
```

{{capability:AdminMintToken}} prints the secret exactly once.
{{capability:AdminListTokens}} shows what exists without showing secrets, and
{{capability:AdminRevokeToken}} ends one.

--scope is required and has no default; there is no token you get by leaving
it out. The vocabulary is eight scopes, six of which the daemon enforces
today:

```
mail.read       list and get messages, threads, attachments
mail.write      move, copy, flag, delete
mail.send       compose and outbox
ai.invoke       summaries, drafts, ask-mailbox
automation      rules, hooks, webhooks
admin           every capability, including minting the next token
ai.spend:<usd>  accepted and stored; no method requires it yet
mailbox:<name>  the same, and it confines nothing
```

The last two are the trap. The scope table is keyed by method name alone, so
it cannot see which mailbox a request names or what a call would cost — a
token minted with only mailbox:INBOX is not restricted to INBOX, it grants
nothing at all, which is fail-safe and reads like success.

--ttl is optional, and leaving it off means the token never expires: the mint
prints the word never on its expires line rather than leaving you to infer
it. A consumer that will not outlive the week costs you less attention as
--ttl 7d than as a revocation you have to remember. All three commands
require admin themselves, since a token that could mint would be a token that
could widen itself.

## The local socket is a different story

Over the Unix socket the daemon reads the peer uid and grants the socket's
owner admin before it looks at any token header, so a token set there narrows
nothing server-side. What narrows an agent on that path is the surface it is
given — the tool list an MCP server is started with — rather than a credential
it presents.

The exception is the password gate: with client_auth.require_for_local set —
it is false out of the box — even a local caller must have logged in, and
mail auth login is what caches the session that later commands present. See
[[daemon]].

## Read-only means two different things

Filtering by scope is what the daemon enforces. The stricter, effect-based
statement is a surface that withholds every tool that changes mail, spends at
a model provider, or produces something that can — and refuses those calls
rather than merely hiding them, so the shorter listing stays true. Neither
means no writes at all: search is read-only by that measure and still appends
to a local learning log, which search.learning ends. That key is true out of
the box, and false is a hard opt-out rather than collection you later ignore:
the rows are never written at all.

## Almost none of this lives in a file

Tokens are rows, not configuration — the [[config-file]] has no table of
them, so mail token list is the whole inventory. The table it prints is id,
name, active or revoked, and scopes; never the secret, and not the expiry
{{capability:AdminMintToken}} showed you once, which --format json carries
along with the last-used timestamp. Revoking marks the row rather than
deleting it, so a token you retired stays visible as revoked.

Once a password exists, the numbers around it come from the client_auth
table: session_ttl is 30d, so a session cached by mail auth login outlives a
day's work and not a year of it, while max_attempts is 5 and lockout is 15m —
Argon2id makes each guess expensive, and only the lockout stops an attacker
with a large request budget from paying that cost over and over. Each takes
an environment override, RMAIL_CLIENT_AUTH__SESSION_TTL and the rest, and no
RPC reads them back. What {{cmd:auth status}} answers is the pair that matters
when a call comes back UNAUTHENTICATED — whether the gate is on, and which
credential this client is presenting.

A client presents its token as RMAIL_TOKEN rather than as --token on a
command line, where a secret is visible in ps and in shell history. Over the
socket that token changes nothing, so --addr is the only path on which it is
the whole of your authority.
