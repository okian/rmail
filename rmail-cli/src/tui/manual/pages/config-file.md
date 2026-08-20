# The config file

One TOML file, read at startup, overlaid by environment variables. Parsing it
is a pure function, so there is no hidden global state and a reload is a
re-parse.

## Where it is

$RMAIL_CONFIG, defaulting to ~/.config/rmail/config.toml. The key bindings
live beside it in their own file — see [[keys-toml]].

## Its shape

```toml
[[accounts]]
name = "Personal"
imap_server = "imap.fastmail.com"
port = 993
username = "user@example.com"
password_command = "security find-generic-password -s fastmail -w"
smtp_server = "smtp.fastmail.com"
smtp_port = 587

[sync]
interval = "5m"
idle = true
qresync = true

[search]
default_mode = "hybrid"
rerank = "auto"
learning = true

[index]
enabled = true
[index.semantic]
provider = "local"

[ai]
enabled = true
provider = "claude"
[ai.limits]
daily_cost_cap_usd = 5.00
[ai.privacy]
redact = true

[send]
undo_window = "10s"

[grpc]
auth = "token"
```

The tables are sync, search, index, ai, tags, notes, send, finder, grpc,
hooks, webhooks, rules, notify, digest, extract, agent, crypto and
client_auth, plus the accounts array.

client_auth, the last of those tables, is the password gate over rmail's own
API — distinct from an account's IMAP credentials, and the one setting that
changes who may talk to the daemon at all. It is managed with mail auth, not
by editing rows; see [[daemon]].

An accounts block does not create an account. The account is a row in the
database with an id, created by {{capability:AccountCreate}}, and that id is
what every other verb takes — [[add-any-account]] is the procedure, and
[[provider-settings]] is the hostname, port and credential each provider will
accept. A block here is matched to that row by name, and what it carries is
the policy no database column holds: ai.enabled false, which stops every
enrichment, embedding and question for that account rather than hiding them,
and notify.threshold, which moves one account's notification bar without
touching the others. An ai.policy rule naming an account that has no block
here is refused when the policy engine is built, so a typo'd name fails
loudly instead of quietly protecting nothing.

Omit the ai subtable and enabled is true. The connection keys a block may also
carry — imap_server, port, username, password_command, password_env, keychain,
smtp_server, smtp_port — are the ones mail account add prints, and the schema
accepts them with 993 and 587 as their defaults, but nothing in the daemon
reads them from this file. They belong to the row, and the Create call is what
writes them; a credential pasted here configures no login. The two notify
fields have no default at all, and that
is deliberate — unset means this account did not say, so the notify table's
own value applies, and raising the global threshold moves every account that
never overrode it.

## Unknown keys are refused

A key the schema does not know is an error naming the key, not something
silently ignored. A typo in a config file that is quietly dropped is a setting
you believe you have set.

## Secrets are never in it

A password is referenced, never written: password_command, password_env, or a
keychain item. OAuth refresh tokens go straight to the keychain and never
appear here at all. See [[add-oauth-account]].

## Environment overlay

Every field can be overridden without editing the file. The rule is uniform,
with a double underscore for nesting:

```
RMAIL_<TABLE>__<FIELD>[__<SUBFIELD>...]

RMAIL_SYNC__INTERVAL=5m
RMAIL_SEARCH__DEFAULT_MODE=hybrid
RMAIL_AI__LIMITS__DAILY_COST_CAP_USD=2.00
RMAIL_INDEX__SEMANTIC__PROVIDER=local
```

Values use the same syntax the TOML does: durations like 5m, booleans,
numbers, and enum keywords. Only variables whose first segment names a known
table are read; anything else is ignored.

## The bootstrap variables are separate

$RMAIL_SOCKET, $RMAIL_DB, $RMAIL_CONFIG and $RMAIL_KEYS are read before the
config system exists, so they are not part of the overlay above and cannot be
set from inside the file. See [[daemon]].

## What lives in the database instead

Rules, webhook destinations, saved searches, smart folders, tags, notes, AI
budgets and capability tokens are live rows, not configuration. They are
created and inspected through their own commands, because they change while
the daemon is running and a file that had to be reloaded to add one would be
the wrong shape for them.

## The defaults, table by table

There is no file on a fresh install and that is not an error: the daemon
starts on every table's built-in default and reads only what a file actually
names. Durations take the units ms, s, m, h and d, and nothing else. These
are the fields people change:

```toml
[sync]
interval = "5m"
poll_interval = "5m"
idle = true
qresync = true

[search]
default_mode = "hybrid"
fusion = "rrf"
rrf_k = 60
rerank = "auto"
learning = true
default_limit = 25
candidates_per_source = 200
top_k_rerank = 50

[index]
enabled = true
workers = 4
batch_size = 64
priority_recent_days = 30
priority_mailboxes = ["INBOX"]

[index.semantic]
enabled = true
provider = "local"
chunk_tokens = 512
chunk_overlap = 64

[ai]
enabled = true
provider = "claude"

[ai.limits]
max_concurrency = 4
requests_per_minute = 60
daily_token_cap = 2000000
daily_cost_cap_usd = 5.00
monthly_cost_cap_usd = 100.00
on_cap = "pause"

[ai.privacy]
redact = true
strip_attachments = true
max_body_chars = 40000

[send]
undo_window = "10s"
workers = 2
max_retries = 5
smtp_security = "auto"
append_to_sent = true
default_timezone = "America/Los_Angeles"

[finder]
enabled = true
default_scope = "all"
max_results = 200
max_entries = 200000
max_memory_mb = 25

[notify]
enabled = false
threshold = "high"
include_subject = true
include_reason = false
max_message_age = "60m"

[grpc]
enabled = true
socket_path = "~/.local/state/rmail/rmaild.sock"
tcp_enabled = false
auth = "token"

[client_auth]
require_for_local = false
session_ttl = "30d"
max_attempts = 5
lockout = "15m"
```

Three tables ship switched off, and each is one whose effects cost money or
leave this machine: notify.enabled, webhooks.enabled and digest.enabled. The
agent ships with allow_mutations false, no labels and an archive-only action
vocabulary for the same reason. hooks.enabled and rules.enabled are on,
because a hook runs a command you wrote on this machine and a rule matches
locally — neither reaches anything you had not already given it.

notify.max_message_age of 60m is what makes turning notifications on safe
rather than expensive. The dispatch loop keeps its cursor in memory and
restarts it at zero, so without that bound the first boot after enabled = true
would pay to score a week of already-read mail and then interrupt you about
all of it.

client_auth.require_for_local is false, which is the daemon's behaviour
before that gate existed: a Unix-socket peer whose uid matches is admin
without presenting a token. Turning it on before a password is configured
would lock out every local client including the one that configures the
password, so the daemon refuses to start in that state rather than letting
you discover it.

The tables not listed above are mostly bounds rather than choices, and four
of their values are worth knowing without looking up: tags.hierarchy_separator
is a forward slash, notes.editor is $EDITOR, rules.archive_mailbox and
agent.archive_mailbox are both Archive, and crypto.keyservers is empty on
purpose — no key lookup goes to a third party until you name one, though
crypto.auto_encrypt is on with policy auto, which encrypts when every
recipient has a usable key and sends in the clear otherwise rather than
blocking the send.

.env.example in the repository root is the long form: a commented environment
line per knob for most of these tables, each carrying the reason the value is
what it is rather than only the value. What the daemon is enforcing now, as
against what the file says, is a different question — {{cmd:ai cost}} prints
the daily and monthly caps actually in force, which is how you tell an edit
that landed from one that did not.
