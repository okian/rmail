# Worked example: add a Gmail account

Gmail and Outlook do not take a password. They take an OAuth token, which
expires, and which has to be refreshed by something that is running.

## Discover the settings

```
mail account add you@example.com
```

{{capability:AccountAutoconfigure}} probes the domain's autoconfig document,
Mozilla's ISPDB, Microsoft autodiscover and the RFC 6186 SRV records, and
prints a ready TOML block. It writes nothing: the block is for you to paste
into the config file, which is where accounts are declared — see
[[config-file]]. That is why the verb is called add and still leaves you with
an editor open.

## Log in

An account exists once the config file names it, and it has an id. Then:

```
mail account login --oauth google --client-id <client-id> 1
```

{{capability:AccountBeginOAuth}} binds a loopback redirect port and prints
the authorization URL; {{capability:AccountCompleteOAuth}} finishes when the
browser comes back. The refresh token is written to the system keychain by
the daemon and never crosses the client process. There is no form of this
that accepts a token on a command line — a secret on a command line is
visible in the process list and in shell history.

## Check it before you trust it

{{capability:AccountTestConnection}} performs a real login and reports the
server's capabilities. It has no mail verb of its own yet, so the generic
client is how you call it:

```
mail api call AccountService.TestConnection '{"id": 1}'
```

It is worth knowing whether the server offers IDLE, CONDSTORE and QRESYNC:
with them, sync transfers only what changed; without them, it falls back to a
UID window diff, which is correct but costs more.

## Then sync

```
mail sync --account 1 --full
```

The first sync walks the mailbox by UID window with resumable checkpoints, so
interrupting it loses nothing. Recent mail and the inbox are fetched first,
which is what makes the client useful before the walk finishes.

## When the token expires

{{capability:AccountRefreshToken}} runs on its own while the daemon is up,
and mail account refresh 1 forces it. A machine that has been asleep for a
month comes back to an expired token and refreshes it on the next attempt;
what you see in the meantime is an UNAUTHENTICATED on that account and a
client that otherwise keeps working — see [[offline]].

## Where to read next

- [[practice-accounts]] — why the second account is often the right answer.
- [[troubleshooting]] — when the login does not come back.
