# rmail

Local-first CLI/TUI IMAP mail client and mail↔AI bridge (MCP + gRPC), built for
macOS. rmail continuously syncs IMAP accounts into a local store, deeply indexes
everything, and exposes every capability to both humans (CLI/TUI) and AI agents
(MCP + a complete gRPC API). See [`prd.md`](prd.md) for the full product spec and
[`tasks.md`](tasks.md) for the work breakdown.

> Status: in progress. The workspace, toolchain gates, gRPC daemon, typed
> config, SQLite store, account/credential handling, IMAP connectivity, message
> fetch/parse/persist, threading, and folder sync (initial + delta) are in
> place. See [`tasks.md`](tasks.md) for what is done and what is next.

## Workspace layout

| Crate         | Role                                                             |
|---------------|-----------------------------------------------------------------|
| `rmail-proto` | Generated protobuf/gRPC types (`rmail.v1`) + descriptor set.     |
| `rmail-core`  | Domain, storage, sync, index, search, AI — shared library.      |
| `rmaild`      | The daemon: background sync/indexing/ranking/AI + gRPC server.   |
| `rmail-cli`   | The `mail` binary — a thin gRPC client.                          |

## Requirements

- Rust (stable), with `rustfmt` and `clippy`
- [`protoc`](https://grpc.io/docs/protoc-installation/) (protobuf compiler)
- [`buf`](https://buf.build/) for proto lint/breaking checks
- [`cargo-nextest`](https://nexte.st/) (preferred test runner)

## Build & run

```sh
cargo build --release

# Start the daemon (listens on $RMAIL_SOCKET, default
# $HOME/.local/state/rmail/rmaild.sock — created with 0600 perms).
./target/release/rmaild

# In another shell: round-trip a gRPC health check.
./target/release/mail ping
# -> rmaild health: Serving
```

The daemon shuts down gracefully on SIGINT/SIGTERM and unlinks its socket.

## Configuration

Environment knobs are documented in [`.env.example`](.env.example). Copy it to
`.env` for local development. No secrets belong in that file.

| Variable            | Meaning                                                       |
|---------------------|---------------------------------------------------------------|
| `RMAIL_SOCKET`      | Unix domain socket the daemon serves and the CLI dials.       |
| `RMAIL_DB`          | SQLite database file the daemon opens (migrations run on open).|
| `RUST_LOG`          | `tracing-subscriber` `EnvFilter` directive for daemon logs.   |
| `RMAIL_LOG_FORMAT`  | Daemon log format: `text` (default) or `json`.                |

Configuration file fields can also be overridden by environment variables of
the form `RMAIL_<TABLE>__<FIELD>` (double underscore = nesting); see
[`.env.example`](.env.example).

All logs go through `tracing` — the daemon never writes to stdout/stderr
directly. Set `RMAIL_LOG_FORMAT=json` for structured logs suitable for a log
shipper.

## Mail sync

Two engines share one folder model, keyed on `(mailbox, UIDVALIDITY, UID)` and
checkpointing into the same `sync_state` row:

- **Initial sync** (`rmail_core::sync::full`) walks a folder's UID space
  downward from `UIDNEXT` in windows, so the newest mail lands first and a
  mailbox is useful long before the download finishes. It is resumable by
  construction — a crash costs at most one window.
- **Delta sync** (`rmail_core::sync::delta`) is the steady state. It answers
  "what changed?", which the UID walk cannot see: a flag flipped on another
  device, or a message expunged elsewhere. It picks the cheapest question the
  server can answer:

  | Strategy | Requires | Cost |
  |---|---|---|
  | `Qresync` | `QRESYNC` + a stored modseq | one `UID FETCH … (CHANGEDSINCE n VANISHED)` — changes *and* expunges |
  | `Condstore` | `CONDSTORE` + a stored modseq | that `FETCH`, plus a `UID SEARCH ALL` for expunges |
  | `UidDiff` | nothing | `UID SEARCH ALL` plus a header-only flag sweep |
  | `Full` | nothing | hands back to the initial walk |

`HIGHESTMODSEQ` is a checkpoint of what has been *applied*, not a reading: a run
that is interrupted leaves it where it was, so the next run re-asks rather than
skipping past an unapplied change. A `UIDVALIDITY` change drops the stale local
copy and rebuilds the folder.

`sync.qresync = false` (or `RMAIL_SYNC__QRESYNC=false`) forces the enumeration
diff on servers whose modseqs cannot be trusted. The engine honors it today via
`ImapCapabilities::without_modseq`; the daemon-side scheduler that reads the
setting lands with `SyncService` (task 15).

## Quality gates

All of these must pass (the CI workflow and the local `Stop` hook enforce them):

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo nextest run --all-features --workspace
buf lint
cargo build --release
```

Production code must not use `unwrap()`, `expect()`, `panic!`, or `todo!()` —
these are denied via workspace clippy lints (test code is exempt).
