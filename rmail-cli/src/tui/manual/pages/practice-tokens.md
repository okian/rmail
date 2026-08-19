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

## The local socket is a different story

Over the Unix socket the daemon reads the peer uid and grants the socket's
owner admin before it looks at any token header, so a token set there narrows
nothing server-side. What narrows an agent on that path is the surface it is
given — the tool list an MCP server is started with — rather than a credential
it presents.

The exception is the password gate: with client_auth.require_for_local set,
even a local caller must have logged in, and mail auth login is what caches
the session that later commands present. See [[daemon]].

## Read-only means two different things

Filtering by scope is what the daemon enforces. The stricter, effect-based
statement is a surface that withholds every tool that changes mail, spends at
a model provider, or produces something that can — and refuses those calls
rather than merely hiding them, so the shorter listing stays true. Neither
means no writes at all: search is read-only by that measure and still appends
to a local learning log, which turning learning off ends.
