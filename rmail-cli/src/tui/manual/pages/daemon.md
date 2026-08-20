# The daemon

Nothing you are looking at talks to your mail server. A background process,
rmaild, owns the IMAP connections, the SQLite database and every AI call; the
screen in front of you is a gRPC client of it, and so is the mail command
line, and so is Claude over MCP. That is the whole reason the same archive
happens whether a key, a command or an agent asks for it.

## Starting and stopping it

```
mail daemon start      # spawn it, wait until the socket answers
mail daemon status     # is it serving, and where
mail daemon stop       # stop the one this machine started
```

This is the only verb in mail that starts a process. Every other verb reports
FAILED_PRECONDITION and names this one, rather than quietly launching a daemon
you did not ask for.

start waits up to 30 seconds for the socket to answer, stop up to 20 for the
process to exit; --timeout on either moves the number. Neither wait is the
whole story when something is wrong — start watches the process it spawned as
well as the socket, so a daemon that exits on a malformed config file is
reported as having exited at the next 50-millisecond poll rather than after
the full 30 seconds.

## Where it lives

- The Unix socket is $RMAIL_SOCKET, defaulting to
  ~/.local/state/rmail/rmaild.sock. It is created 0600, which is
  defence in depth rather than the gate — the gate is the kernel's own
  peer-credential check on the connection.
- The database is $RMAIL_DB, defaulting to ~/.local/state/rmail/rmail.db —
  a sibling of the socket. Migrations run at open, idempotently.
- Configuration is $RMAIL_CONFIG, defaulting to ~/.config/rmail/config.toml.
  See [[config-file]].

All three are read before the config file is parsed — the daemon has to open
its socket and its database before it has read a word about either — so they
are not part of the RMAIL_ overlay the config tables take, and cannot be set
from inside config.toml. The grpc table does carry a socket_path with the same
default string, but what rmaild binds is the environment path, which mail
daemon start hands to the process it spawns.

## What the TUI does when the daemon is down

It starts anyway, and says so. Local screens keep working: this manual is
compiled into the binary and reads nothing, and key bindings are read from a
file rather than fetched over gRPC, so rebinding works with nothing running —
the file is re-read once a second, which is why a chord you save in another
window is live before you have switched back to this one. Anything that needs
mail reports the failure on the status line instead of tearing the screen
down. [[offline]] draws the same line from the other side — daemon running,
network gone.

## Watching it from inside the client

The client asks four questions every five seconds — is sync paused, what is the
index queue doing, is the AI loop running, how much has been spent today — and
draws the answers as the four indicators on the bottom row ([[tour]] lists the
glyphs). They are a poll rather than a subscription because none of those four
pushes; where something does push, the client uses that instead. Reloading the
folder list answers the sync question on the way past, so the indicator is fresh
the moment a reload happens rather than up to five seconds later.

The poll is deliberately outside the busy marker. That marker counts work *you*
asked for, and a heartbeat incrementing it would leave it lit forever — which
would make the one signal it carries useless.

## Trusting the local socket

Over the Unix socket the daemon reads the connecting process's uid from the
kernel and grants the socket's owner admin. A capability token narrows nothing
on that path; it is the TCP listener that has no uid to trust, which is where
{{capability:AdminMintToken}} and its scopes start to matter.
[[practice-tokens]] is the short version.

That grant is not unconditional. A password gate over rmail's own API turns
it off — with client_auth.require_for_local set, a local caller must log in
like any other:

```
mail auth setup      set the password
mail auth login      prove it, and cache a session for later commands
mail auth status     whether one is set, and whether local callers need it
```

require_for_local is false out of the box, and no password is configured, so
on a fresh install the uid check above is the whole of the answer. Setting it
true with nothing configured is refused at startup rather than at the next
connection, which is what stops you locking out the only client that could
set a password. Once one is set, a session from mail auth login lasts 30 days,
and five wrong guesses lock further attempts out for 15 minutes.

From inside the client, {{cmd:auth status}} answers the same question without
quitting, and adds the half the daemon cannot know: which credential this
client is presenting. It draws that as a [[reports]] screen, and the row for a
configured password offers {{cmd:auth clear}} on Enter.

## The numbers, and which ones you can change

The daemon's own table in the [[config-file]] is grpc, and a fresh install
serves on these:

```toml
[grpc]
enabled = true
socket_path = "~/.local/state/rmail/rmaild.sock"
listen = ""
tcp_enabled = false
auth = "token"

[grpc.limits]
max_message_bytes = 16777216
max_concurrent = 256
stream_buffer = 1024
request_timeout_secs = 120

[grpc.events]
retention_days = 7
retention_rows = 1000000

[grpc.idempotency]
retention = "1d"
in_flight = "5m"

[grpc.web]
enabled = false
cors_origins = ["http://localhost:5173"]
```

Three of those blocks carry more than their names suggest. tcp_enabled is
false and listen is empty, so on a fresh install the Unix socket is the only
way in — the TCP listener the section above describes does not exist until
you configure one, and auth is the mode it will demand when it does. The two
events numbers both apply, whichever bites first: a client disconnected for
longer than seven days, or than a million rows of event log, is told
OUT_OF_RANGE and resyncs rather than resuming where it stopped. And
idempotency.retention is the window in which a retried Move or Delete replays
its recorded answer instead of applying twice — a day, while a claim that
never reported an outcome is fenced for five minutes, because a client whose
deadline elapsed mid-call retries the same key almost immediately.

Every field above takes an environment override with the same shape —
RMAIL_GRPC__LIMITS__MAX_CONCURRENT, and
RMAIL_CLIENT_AUTH__REQUIRE_FOR_LOCAL for the password gate — so a one-off
does not need a config.toml edit.

The client's own intervals are the exception: the five-second heartbeat, the
one-second re-read of the key bindings file and the hundred-millisecond wait
for a keypress are compiled into the binary, with no config key and no
environment variable. Five seconds is four local reads over a Unix socket,
which costs the daemon almost nothing; a second is the longest anybody
editing a binding in the next window will believe the file was read; and 100
milliseconds is how long quitting can take to be noticed, because the input
thread checks whether the session is over between polls rather than while
blocked in one.

## Where to read next

- [[troubleshooting]] — the symptoms this page's answers usually arrive as.
- [[offline]] — what still works with the network gone.
- [[reports]] — the screen every row-shaped answer arrives on.
- [[daemon-control]] — the commands that read and change what it is doing.
