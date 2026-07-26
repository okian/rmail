# rmail

Local-first CLI/TUI IMAP mail client and mail↔AI bridge (MCP + gRPC), built for
macOS. rmail continuously syncs IMAP accounts into a local store, deeply indexes
everything, and exposes every capability to both humans (CLI/TUI) and AI agents
(MCP + a complete gRPC API). See [`prd.md`](prd.md) for the full product spec and
[`tasks.md`](tasks.md) for the work breakdown.

> Status: early scaffold. The workspace, toolchain gates, and a minimal gRPC
> daemon (health + reflection over a Unix domain socket) are in place.

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
| `RUST_LOG`          | `tracing-subscriber` `EnvFilter` directive for daemon logs.   |
| `RMAIL_LOG_FORMAT`  | Daemon log format: `text` (default) or `json`.                |

Configuration file fields can also be overridden by environment variables of
the form `RMAIL_<TABLE>__<FIELD>` (double underscore = nesting); see
[`.env.example`](.env.example).

All logs go through `tracing` — the daemon never writes to stdout/stderr
directly. Set `RMAIL_LOG_FORMAT=json` for structured logs suitable for a log
shipper.

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
