# Worked example: add any account

Five steps, in this order, whatever is on the other end of the address:

- Discover the settings.
- Choose the credential the provider will accept.
- Create the account.
- Authorize it, if the provider uses OAuth.
- Check it, then sync.

Only the second and fourth vary by provider. [[provider-settings]] is the
table of which credential each one takes; this page is the procedure the table
feeds. Nothing below asks you to know a hostname — the first step finds it.

## Discover the settings

```
mail daemon start
mail account add you@example.com
```

{{capability:AccountAutoconfigure}} probes four sources in order: the domain's
own autoconfig document, Mozilla's ISPDB, Microsoft autodiscover, and the RFC
6186 SRV records. It stops at the first one that answers with something it can
parse, and then validates that — so a domain publishing a broken autoconfig
document fails the discovery rather than falling through to the ISPDB. The
output names the source, and ends with the block itself:

```
imap: imap.fastmail.com:993 (tls) as you@example.com   [source: autoconfig]
smtp: smtp.fastmail.com:587 (starttls)
login: not verified: no credential reference was supplied, so no login was attempted

# Discovered by `mail account add` via autoconfig.
# Nothing was written; paste this into rmail.toml to apply it.
[[accounts]]
name = "you@example.com"
imap_server = "imap.fastmail.com"
port = 993
username = "you@example.com"
smtp_server = "smtp.fastmail.com"
smtp_port = 587
```

Three things about that are worth knowing before you rely on it.

It writes nothing. No account is created and no existing account is changed —
there is no code path from a discovery to an update, which is what makes
"autoconfig cannot silently downgrade my connection" a property of the design
rather than a rule somebody has to remember. When an account already exists
for the address, the response names its id and puts the differences in
warnings for you to read.

Every value in it is someone else's text: a domain administrator's, or a
third-party database's. They are not ranked by trust, because the same
validator stands between all of them and a connection — a syntactically valid
public hostname, a port in range, and an encrypted transport.

And it did not log in, because nothing was given to log in with. Supply a
credential reference and it does:

```
mail account add you@example.com \
  --password-command "security find-generic-password -s fastmail -a you@example.com -w"
```

That prints login: verified, or the server's own refusal. It is the difference
between settings that parse and settings that work, and it costs one round
trip.

If all four probes miss — a small host with no autoconfig document and no SRV
records — then --ai lets Claude propose settings from the domain, its MX
records and whatever the probes did return. Two things about that path:

- It is never logged into, even with a credential. Validation proves a name is
  syntactically a public hostname; it cannot prove the name is your provider,
  and presenting your password to a host a model produced from
  attacker-controlled probe responses is not something consent to *ask* the
  model extends to. The output says so.
- The refusal it prints tells you to run mail account test, and there is no
  such verb. The check is the one at the bottom of this page.

It also costs money — see [[ai-cost]] — and the answer is a guess.

## Choose the credential the provider will accept

Four forms, and none of them is a secret. Every one is a *reference* the
daemon resolves when it needs a password, and none is ever persisted:

```
{"password_command": "security find-generic-password -s fastmail -a you@example.com -w"}
{"password_env": "FASTMAIL_APP_PASSWORD"}
{"keychain": "fastmail"}
```

- password_command runs a shell command and takes its trimmed stdout. It runs
  inside the daemon with no stdin and no stderr, and is killed after ten
  seconds — so a pass or gpg setup that wants to prompt on a terminal fails
  rather than asking. A GUI agent that can answer within ten seconds
  (pinentry-mac, a biometric prompt) works; a tty pinentry does not.
- password_env reads a named environment variable, in the daemon's
  environment. What you configure is the variable's name, never its value.
  Suited to a container or a service manager that already holds the secret.
- keychain looks up a macOS generic-password item by service name, with the
  account's username as the item's account field. Both halves are part of the
  key, so the item has to be created with both:

```
security add-generic-password -s fastmail -a you@example.com -w
```

  Omit -a and every login fails with a keychain lookup error, because the
  lookup asks for a pair. This form can prompt too — macOS raises its own
  dialog when the daemon is not in the item's access list.
- The fourth form is OAuth, and it is the one you do not write. The account
  authenticates with a short-lived bearer token over SASL XOAUTH2, and the
  reference is a keychain service name the daemon chooses and writes itself —
  see the step below.

Which of the four a provider will accept is not your choice to make. Microsoft
takes OAuth and only OAuth: basic authentication for IMAP and SMTP is retired
across Exchange Online and the consumer service. Google takes either — OAuth,
or an app password on a personal account with 2-Step Verification switched on.
iCloud, Yahoo and Fastmail take a password, but only an app-specific one
generated in their web interface; the account password is refused by design. A
server you run takes whatever you configured. [[provider-settings]] is the
row-per-provider version, with the page each one keeps its answer on.

## Create the account

The block the discovery printed is configuration. The account is a row in the
database with an id, and the id is what every other verb takes.
{{capability:AccountCreate}} is what makes one:

```
mail api call AccountService.Create '{
  "name": "Fastmail",
  "imap_server": "imap.fastmail.com",
  "imap_port": 993,
  "username": "you@example.com",
  "smtp_server": "smtp.fastmail.com",
  "smtp_port": 587,
  "credential": {"keychain": "fastmail"}
}'
```

There is no mail subcommand for it, so from a shell the generic client is how
you reach it — the same daemon and the same auth layer, with proto field names
rather than camelCase. Inside the client the same call has a verb:
{{cmd:account new}} takes a name and the same fields as flags, and
{{cmd:account list}} shows what exists.

```
:account new Fastmail --imap-server=imap.fastmail.com --keychain=fastmail
```

From a shell, {{capability:AccountList}} is the listing:

```
mail api call AccountService.List
```

Omit imap_port and 993 is used; omit smtp_port and 587 is. Omit the credential
and the account exists and cannot log in, which is a legitimate intermediate
state — it is what an OAuth account looks like until the next step runs.

Get a hostname or a username wrong and there is no fixing it in place: the
service has Create, Get, List, Delete, TestConnection, Autoconfigure and the
three OAuth calls, and no Update. A wrong row is deleted and made again —
{{cmd:account rm}} in the client, or:

```
mail api call AccountService.Delete '{"id": 1}'
```

## Authorize it, if the provider uses OAuth

```
mail account login --oauth google --client-id <client-id> 1
```

{{capability:AccountBeginOAuth}} binds a loopback redirect port and prints the
authorization URL; {{capability:AccountCompleteOAuth}} finishes when the
browser comes back. PKCE throughout, so a code observed on that socket is
useless to whatever observed it.

The client id is yours rather than rmail's: you register a desktop application
with the provider once and reuse it. It is not a secret. Google issues desktop
clients a client secret as well and requires it, and
--client-secret-command takes a command whose stdout is that secret, so it is
never an argument either.

The account has to have a username before this runs, and the flow refuses
without one rather than writing a grant to a keychain item nothing could
address afterwards. When the browser comes back, the daemon writes the refresh
token to the system keychain — under a service name derived from the account
id, rmail-oauth-google-1 for account 1 — and sets the account's credential to
OAuth itself. You do not name that item and cannot get it wrong. The token
never crosses the client process, and there is no form of this verb that
accepts one as an argument: a secret on a command line is visible in the
process list and in shell history.

[[add-oauth-account]] follows this step through in detail, scopes included.

## Check it, then sync

For a password account, {{capability:AccountTestConnection}} performs a real
login and reports what the server offers. {{cmd:account test}} inside the
client, or from a shell:

```
mail api call AccountService.TestConnection '{"id": 1}'
```

For an OAuth account it is not the check to use — it resolves a password, and
an OAuth account has none, so it refuses before reaching the network. There
the first sync is the check:

```
mail sync --account 1 --full
```

Either way, read what comes back rather than glancing at it. With IDLE,
CONDSTORE and QRESYNC, sync transfers only what changed; without them it falls
back to a UID window diff, which is correct and costs more. The first pass
walks the mailbox by UID window with resumable checkpoints, so interrupting it
loses nothing, and recent mail and the inbox come first — which is what makes
the client useful before the walk finishes. {{cmd:sync status}} says how far
it has got and {{cmd:sync now}} asks for another pass.

Then open it:

```
mail tui
```

[[tour]] is the screen that appears.

## What the config file adds

The row is the account. A [[config-file]] block is *policy* over it, matched to
the row by name, and only three things in one are read: the name, the ai table
and the notify table.

```toml
[[accounts]]
name = "Fastmail"

[accounts.ai]
enabled = false

[accounts.notify]
threshold = "high"
```

The connection keys the discovery prints — imap_server, port, username,
password_command, smtp_server, smtp_port — are accepted by the parser and
read by nothing. They are the row's business, and the row is what the Create
call above wrote. Pasting a credential into the file does not configure one.

What the block does do is worth having. An account with ai.enabled false is
not an account whose AI features are hidden: nothing enriches it, nothing
embeds it, and no question can reach it. [[practice-accounts]] is why that is
usually the reason to have a second account at all.

## A second account is the same five steps

Nothing here is global. Each account resolves its own credential, keeps its
own notification threshold, and finds its own archive folder. Search, tags and
the finder stay unified across all of them, because the reason to split
accounts is policy rather than retrieval — {{capability:MailListUnified}} is
the merged inbox that follows from the same choice.

## When it does not work

- A login refused with the password that works in a browser: the provider
  wants an app-specific password, or OAuth. [[provider-settings]] first.
- A TLS handshake that fails against a server you run: the certificate has to
  chain to a public root, and there is no per-account CA setting to add yours
  to.
- One account UNAUTHENTICATED while everything else keeps working: an OAuth
  token expired while the machine was asleep. {{capability:AccountRefreshToken}}
  runs on its own while the daemon is up; mail account refresh 1 asks for it
  now, and mail account refresh 1 --force is what refreshes a token the daemon
  has already written off.
- [[troubleshooting]] tells a daemon that is not running apart from a login
  that is not working, which look alike on the status line.

## Where to read next

- [[provider-settings]] — hostnames, ports, and where each provider keeps the
  thing you have to fetch.
- [[accounts-and-tokens]] — the same ground from inside the client, plus
  switching between accounts and the tokens that let anything else call this
  daemon.
- [[add-oauth-account]] — the OAuth flow end to end.
- [[practice-accounts]] — one account per trust boundary, and why.
- [[tour]] — the screen your mail arrives on.
