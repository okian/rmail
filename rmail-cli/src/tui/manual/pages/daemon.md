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

## Where it lives

- The Unix socket is $RMAIL_SOCKET, defaulting to
  ~/.local/state/rmail/rmaild.sock. It is created 0600, which is
  defence in depth rather than the gate — the gate is the kernel's own
  peer-credential check on the connection.
- The database is $RMAIL_DB, defaulting to ~/.local/state/rmail/rmail.db —
  a sibling of the socket. Migrations run at open, idempotently.
- Configuration is $RMAIL_CONFIG, defaulting to ~/.config/rmail/config.toml.
  See [[config-file]].

## What the TUI does when the daemon is down

It starts anyway, and says so. Local screens keep working: this manual is
compiled into the binary and reads nothing, and key bindings are read from a
file rather than fetched over gRPC, so rebinding works with nothing running.
Anything that needs mail reports the failure on the status line instead of
tearing the screen down. [[offline]] draws the same line from the other side —
daemon running, network gone.

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

From inside the client, {{cmd:auth status}} answers the same question without
quitting, and adds the half the daemon cannot know: which credential this
client is presenting. It draws that as a [[reports]] screen, and the row for a
configured password offers {{cmd:auth clear}} on Enter.

## Where to read next

- [[troubleshooting]] — the symptoms this page's answers usually arrive as.
- [[offline]] — what still works with the network gone.
- [[reports]] — the screen every row-shaped answer arrives on.
