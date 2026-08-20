# Accounts and tokens

Two kinds of credential that are easy to confuse. An *account* is a mailbox on
somebody else's server, and its credential is an IMAP password or an OAuth grant.
A *token* is a bearer secret for rmail's own API — what a script, a CI job or an
agent presents to this daemon. Nothing you do to one affects the other.

## Which accounts exist, and which one you are looking at

{{cmd:account list}} is every account the daemon knows, with its login, its IMAP
endpoint and where its password comes from — never the password, which does not
cross this API at all. The row for the account currently on screen is drawn in the
`ok` tone, so "which one am I looking at" is answerable at a glance.

{{cmd:account use}} switches. Enter on a listing row does the same thing, which is
the shortest path: read the list, move to a row, press Enter. Everything on screen
belongs to the account it came from — folders, the message list, the open message,
the analysis panel, a visual selection — so switching clears all of it and loads
the new account's folders. It is not a restart: the session, the keymap, the
command history and the `:` line's own state are untouched.

An id the daemon has never listed is refused rather than sent. A folder listing
for an account that does not exist answers `NOT_FOUND` two round trips later, by
which point the screen has already been cleared for it.

{{cmd:account show}} is one account's settings in full, including which of
`command`/`env`/`keychain`/`oauth` its credential comes from. With no id it shows
the account on screen.

## Adding one

{{cmd:account add}} discovers an address's settings and reports a proposal. It
writes nothing:

```
mail account add you@example.com
```

The probes run in order — the domain's own autoconfig document, Mozilla's ISPDB,
Microsoft autodiscover, RFC 6186 SRV records — and everything that comes back is
treated as untrusted: hostnames must be real public DNS names, ports must be in
range, and a server offering only an unencrypted connection is refused outright
rather than reported with a warning.

Three rows in that report are worth reading before anything else.

`source` says which probe answered. If it says `model`, every probe missed and a
model *proposed* these settings from the domain, its MX records and the probe
responses. That only happens if you passed `--ai`, it costs money, and the answer
is a guess — validated as strictly as a probe's, and still a guess. The row is
drawn as a warning for that reason.

`verified` says whether a real IMAP login succeeded. It can only say yes if you
supplied a credential, which is what `--password-command`, `--password-env` and
`--keychain` are for — the reference travels, never the secret:

```
mail account add you@example.com --password-command "pass mail/example"
```

`existing` appears when an account is already configured for that address. The
proposal is then explicitly *about* that account rather than a replacement of it,
and nothing was changed.

Two rows at the bottom are how a proposal gets applied. `toml` opens the ready
TOML block in whatever your machine opens `.toml` files with, so it can be pasted
into `rmail.toml` — which is where accounts are normally declared, see
[[config-file]]. That block is an entry in the config file's accounts array:

```
[[accounts]]
name = "you@example.com"
imap_server = "imap.example.com"
imap_port = 993
```

{{cmd:account toml}} opens the same block later, after the report has been
closed.

`apply` runs {{cmd:account new}}, which stores the account through the API
instead. The row carries the whole line with the discovered settings on it, so
what is about to be stored is visible before it runs — and because creating an
account is a real change, it asks first. You can type the line yourself:

```
mail api call AccountService.Create '{"name":"you@example.com","imap_server":"imap.example.com","imap_port":993}'
```

`new` rather than `create` to sit next to `:tag new`; `AccountService.Create` has
no `mail` verb at all, so this is its first surface anywhere.

## Logging in, and staying logged in

Gmail and Outlook do not take a password. {{cmd:account login}} runs the whole
loopback-redirect OAuth flow:

```
mail account login --oauth google --client-id <client-id> 1
```

`--client-id` is the id of a *native* application you registered with the
provider; it is not a secret. Providers that also want a client secret take
`--client-secret-command`, whose stdout is the secret — the secret itself never
crosses this API, only how to obtain it.

The report shows the authorization URL and then waits. The URL is drawn even
though the client hands it straight to your browser, because a browser that does
not launch, or launches somewhere you are not signed in, leaves the URL as the
only way to finish. `--no-browser` skips the launch entirely. The refresh token is
written to the system keychain by the daemon and never crosses this process; there
is no form of this that accepts a token as an argument, because an argument is
visible in the process list.

{{cmd:account refresh}} renews the access token. The daemon does this on its own
while it is up, so the interesting case is the one where it has not: a machine
that has been asleep for a month comes back to an expired token, and the report
says whether this call actually went to the provider or the stored token was still
good. `--force` refreshes anyway.

{{cmd:account test}} performs a real login and reports what the server offers. It
is worth knowing whether IDLE, CONDSTORE and QRESYNC are there: with them, sync
transfers only what changed.

## Removing one

{{cmd:account rm}} deletes an account **and every message stored locally for
it**. It is the one account verb that asks first, and the one that will not take
the account on screen as a default — every other verb here falls back to it, and a
line that deleted whatever happened to be open because its id was left off is a
line nobody should be able to type by accident.

## Tokens for rmail's own API

{{cmd:token list}} is metadata only. That is not a policy this client applies —
`ListTokens` cannot return a secret, because only an argon2id hash of it is ever
stored. A revoked token stays in the list, drawn dim: knowing a token existed and
was revoked is part of an audit trail.

{{cmd:token create}} mints one:

```
mail token create --name ci --scope mail.read --scope ai.invoke --ttl 90d
```

`--scope` takes a comma-separated list or repeats, and at least one is required —
a token with no scopes could never do anything. `--ttl` takes `24h`, `90d` and the
rest of the same spellings the config file uses; leave it off for no expiry, and
note that `0` is not a spelling of that.

Two scopes are accepted and stored but **not yet enforced**: `ai.spend:<usd>` and
`mailbox:<name>`. The daemon's per-method scope table can see which method is
being called, not a request's dollar amount or target mailbox. A token minted with
only `mailbox:INBOX` therefore grants nothing at all today — fail-safe, but do not
rely on it as a restriction.

The report shows the bearer secret, and that is the only time anything will. The
row below it says so, because a reader who does not know will close the pane and
the secret is then gone for good — the daemon cannot show it again, and neither can
this client, which keeps it in that pane and nowhere else. `r` on that report is
refused rather than obeyed: `r` means "ask this verb again", and asking this one
again mints a second token.

{{cmd:token revoke}} revokes by id. It does not ask, unlike `:account rm` —
revoking is the safe direction, nothing is lost that was not already
unrecoverable, and re-revoking an already-revoked token is explicitly not an
error.

## Where to read next

- [[add-oauth-account]] — the same OAuth flow as a worked example, from the CLI.
- [[config-file]] — where accounts are normally declared, and why.
- [[practice-accounts]] — why the second account is often the right answer.
- [[reports]] — the screen these draw into, and what Enter does on a row.
