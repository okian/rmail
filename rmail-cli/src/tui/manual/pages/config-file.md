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

The last of those is the password gate over rmail's own API — distinct from
an account's IMAP credentials, and the one setting that changes who may talk
to the daemon at all. It is managed with mail auth, not by editing rows; see
[[daemon]].

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
