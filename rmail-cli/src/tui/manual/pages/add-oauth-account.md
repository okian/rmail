# Worked example: add a Gmail account

Gmail and Outlook do not take a password. They take an OAuth token, which
expires, and which has to be refreshed by something that is running.

[[add-any-account]] is the same five steps for any provider, and
[[provider-settings]] is where the hostnames and the client-registration
pages live. This page is the OAuth half, followed all the way through.

## Discover the settings

```
mail account add you@example.com
```

{{capability:AccountAutoconfigure}} probes the domain's autoconfig document,
Mozilla's ISPDB, Microsoft autodiscover and the RFC 6186 SRV records, and
prints a ready TOML block. For Gmail the settings you need are the pair rmail
carries in its own provider table:

```
imap.gmail.com:993
smtp.gmail.com:587
```

It writes nothing, and it creates nothing: the block is settings for you to
paste into the config file — see [[config-file]]. That is why the verb is
called add and still leaves you with an editor open.

## Log in

Naming an account in the config file does not give you an account. The row
with the id every verb below takes is created by {{capability:AccountCreate}},
and the config file's accounts array is the policy over that row, matched to
it by name — a name matching no row configures nothing. The whole procedure,
the create included, is on [[add-any-account]]. With the account created:

```
mail account login --oauth google --client-id <client-id> 1
```

{{capability:AccountBeginOAuth}} binds a loopback redirect port and prints
the authorization URL; {{capability:AccountCompleteOAuth}} finishes when the
browser comes back. The redirect is http://127.0.0.1 on an ephemeral port,
path /rmail/oauth/callback, and it is bound when the flow starts rather than
when the browser returns — the port in the URL is one this process already
holds, so no other local process can take it first. The flow uses PKCE
throughout — an S256 challenge over a 32-byte verifier — which is what makes
a code observed on that socket useless to whatever observed it. It waits 300
seconds for consent and then releases the port and the verifier, so a
forgotten browser tab costs you a rerun rather than a daemon holding a socket
for the rest of the day.

The refresh token is written to the system keychain by the daemon and never
crosses the client process. There is no form of this that accepts a token on
a command line — a secret on a command line is visible in the process list
and in shell history.

## Check it before you trust it

{{capability:AccountTestConnection}} performs a real login and reports the
server's capabilities. It has no `mail` verb of its own yet; in the TUI it is
{{cmd:account test}}, and outside it the generic client is how you call it:

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

A token is treated as spent 120 seconds before the expiry it states, because
the IMAP handshake it is about to be used for takes longer than the last
seconds of a token's life, and that failure arrives mid-sync as an
authentication error rather than as a refresh. Two more numbers cover a
provider being unhelpful: a token response that states no lifetime at all is
assumed to last 3600 seconds rather than forever, and a stored expiry more
than 24 hours out is read as evidence of a wrong clock and refreshed instead
of trusted. mail account refresh 1 --force refreshes a token that has not
expired yet.

## What is built in, and what you have to fetch

Google's two OAuth endpoints are compiled in, and no config key and no
environment variable points them anywhere else:

```
https://accounts.google.com/o/oauth2/v2/auth
https://oauth2.googleapis.com/token
```

The mail endpoints are not like that. rmail carries imap.gmail.com:993 and
smtp.gmail.com:587 in the same provider table, but what a sync connects to is
the account row's own imap_server and smtp_server, on its port and smtp_port
— which default to 993 and 587 in the [[config-file]], and are read as 993
and 587 when the row carries no port at all. [[provider-settings]] has the
hostnames for every other provider, Microsoft's included.

Omit --scope and each provider's mail-only default is requested. For Google
that is the single coarse https://mail.google.com/, which is the only Google
scope that grants IMAP and SMTP at all: gmail.readonly and gmail.modify
reach the REST API only, and an IMAP AUTHENTICATE XOAUTH2 carrying one of
them fails. For Microsoft it is offline_access — without which no refresh
token is issued and the grant dies with the access token — plus
IMAP.AccessAsUser.All and SMTP.Send under outlook.office.com. Neither list
asks for profile or contact data. Google's authorization URL also carries
access_type=offline and prompt=consent, which is what makes a second
authorization for an already-consented client return a refresh token rather
than an access token alone.

The client id is the one value rmail cannot supply. There is no default, no
config key and no environment variable for it, and --client-id is required:
you register a desktop application with the provider once and reuse it. For
Google that is console.cloud.google.com, under APIs and Services,
Credentials, as an OAuth client of type Desktop app. Google issues that
client type a client secret and requires it, which is what
--client-secret-command is for; the client id itself is not a secret. The
refresh token is the mirror image — no config key and no environment
variable either, by design — and the keychain item holding it is named from
the account id, rmail-oauth-google-1 for account 1, so renaming the account
cannot orphan the grant.

## Where to read next

- [[accounts-and-tokens]] — the same ground from inside the TUI, plus the
  capability tokens that let anything else call this daemon.
- [[practice-accounts]] — why the second account is often the right answer.
- [[troubleshooting]] — when the login does not come back.
