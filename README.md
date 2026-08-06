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

The `onnx` feature (on by default) builds the local embedding backend, which
links ONNX Runtime. `cargo build --no-default-features -p rmail-core` skips it;
semantic search then degrades to deterministic hashed vectors.

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

- **Watch** (`rmail_core::sync::idle`) decides *when* to ask. It parks a
  long-lived connection on IMAP `IDLE` (RFC 2177) so the server speaks the
  moment something happens, runs a delta pass on every wake-up, and reissues
  `IDLE` on its own cadence so a server with an inactivity timeout does not log
  it off. A server without `IDLE` gets interval polling instead — same loop,
  same delta pass, worse latency. `watch_folders` gives connections to an
  account's highest-priority folders up to a budget, because servers cap
  concurrent connections per account.

  A watch treats disconnection as routine: it reconnects with exponential
  backoff and only gives up on failures that cannot improve with time (a
  deleted folder, revoked credentials). A server that is merely down is
  retried indefinitely at the backoff ceiling — a watch that gave up during an
  outage is a mailbox that silently stops receiving mail.

`HIGHESTMODSEQ` is a checkpoint of what has been *applied*, not a reading: a run
that is interrupted leaves it where it was, so the next run re-asks rather than
skipping past an unapplied change. A `UIDVALIDITY` change drops the stale local
copy and rebuilds the folder.

`sync.qresync = false` (or `RMAIL_SYNC__QRESYNC=false`) forces the enumeration
diff on servers whose modseqs cannot be trusted. The engine honors it today via
`ImapCapabilities::without_modseq`; the daemon-side scheduler that reads the
setting lands with `SyncService` (task 15).

## Search index

Extraction (`index_content`) feeds three retrievers, each of which knows one
thing the others do not:

| Retriever | Finds | Backing |
|---|---|---|
| Lexical | the words you remember | contentless FTS5, field-weighted BM25 |
| Entity | the invoice number, the address, the parcel | `entities` + co-occurrence graph |
| Semantic | the message whose words you *don't* remember | `sqlite-vec` kNN over chunk and message vectors |

Text is split into overlapping chunks at the strongest separator inside the
size budget — blank line, then line break, then sentence end, then word — so a
vector describes a passage rather than the average of a whole thread, and a
result can quote the paragraph that matched. Every chunk carries a byte span
into the source text, so citations quote the message rather than a copy.

Each chunk gets a vector; each message gets the normalized mean of its chunks',
which is what `mail similar` searches. Chunk vectors answer "which passage is
about this"; a message vector answers "which message is like this one", and
ranking chunks and deduplicating afterwards cannot answer the second — a long
thread wins simply by having more chances to match.

Work is keyed on content: a chunk whose text hash and model are both unchanged
is not re-embedded, so re-indexing an unchanged mailbox costs a few hash
comparisons. `index verify` reconciles chunks, vectors and bookkeeping in both
directions, because a `vec0` virtual table takes no foreign key and will not
check it for us.

### Attachments

Attachment text lands in the same place a body does, so the lexical index, the
entity extractor and the chunker reach it through paths they already have. PDF
(with per-page offsets, so a citation can name the page), DOCX, XLSX, PPTX,
HTML, CSV and plain text.

Every input here is a file a stranger sent, so the extractors run on an isolated
task whose panic is caught and whose runtime is bounded — `pdf-extract` alone
has around a hundred panicking call sites, and taking the daemon down over one
attachment takes mail down for every account. Cell counts, page counts,
decompressed bytes, output size and concurrency are all capped: without them, a
1.4 KB spreadsheet allocates gigabytes and a half-megabyte PDF burns a minute of
CPU.

Failure is recorded rather than retried. An encrypted PDF, an unreadable format,
a file past `max_attachment_mb` — each legitimately yields no text, and a row
saying so is what stops the pipeline re-opening the same archive on every pass.
Only a hard extractor failure gets another attempt; a timeout does not, because
a file that takes a minute takes a minute every time.

### Embeddings

Three backends behind one trait, forming a ladder rather than a menu:

| `index.semantic.provider` | Backend | Egress |
|---|---|---|
| `local` (default) | `bge-small-en-v1.5` via ONNX Runtime, 384d | none |
| `voyage` | Voyage AI, key from `api_key_command` | every indexed body |
| `none` | deterministic hashed features | none |

The local model is the default because mail is the most sensitive corpus most
people own, and a hosted embedding API sees effectively all of it. The hashed
fallback is not semantic and does not pretend to be — it exists so that a daemon
with no model still produces vectors of the right shape, and the retrieval
pipeline above it has one code path instead of two.

Provisioning the weights is an explicit act. `index.semantic.local.allow_download`
is **off** by default: a backend whose whole point is that nothing leaves the
host must not contact Hugging Face the first time somebody searches, and the
downloader ignores `HF_HUB_OFFLINE`, so the check has to live here. Turn it on
once to fetch, or populate the cache out of band and point `RMAIL_MODEL_CACHE`
(or `index.semantic.local.cache_dir`) at it.

The daemon warms the model in the background at start and holds it for its
lifetime, so the first user query does not pay for the load. A warm-up failure
degrades search rather than stopping the daemon.

Every vector leaves the boundary unit-normalized, so cosine similarity is a dot
product and nothing downstream carries a normalization step it could skip.

## Daemon surface

| Service | RPCs |
|---|---|
| `grpc.health.v1.Health` | `Check` (reports `SERVING`) |
| `rmail.v1.AccountService` | `Create` `List` `Get` `Delete` `TestConnection` |
| `rmail.v1.SyncService` | `SyncFolder` `Status` `Pause` `Resume` `WatchEvents` |

`WatchEvents` is a server-stream over the durable event log. It subscribes to
the live tail *before* reading the backlog, so a client resuming from a cursor
sees everything it missed and everything that follows with nothing falling
between the two. A cursor older than retention fails with `OUT_OF_RANGE`
carrying `oldest_seq` and `resume_from` in `ErrorInfo` metadata rather than
silently returning an empty stream.

```sh
mail sync --account 1              # delta pass over every folder
mail sync --account 1 --full       # force the initial UID-window walk
mail sync --account 1 --watch      # sync, then follow the event stream
```

## Quality gates

All of these must pass (the CI workflow and the local `Stop` hook enforce them):

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
scripts/docker-test.sh                  # the test suite — see below
buf lint
cargo build --release
```

### Tests run in a container

`scripts/docker-test.sh` builds `Dockerfile.test` and runs
`cargo nextest run --locked --all-features --workspace` inside a container that is
destroyed on exit. Run it instead of `cargo test`/`cargo nextest`; extra cargo
arguments pass straight through:

```sh
scripts/docker-test.sh                        # the whole suite
scripts/docker-test.sh -p rmail-core config:: # narrow it
scripts/docker-test.sh --no-default-features  # the degraded, no-ONNX build
scripts/docker-test.sh --shell                # a shell in the same environment
scripts/docker-test.sh --clean                # drop the cache volumes and image
```

The container is disposable; the cargo registry and the Linux `target/` directory are
not — they persist in named `rmail-test-*` volumes so runs stay incremental, since a
cold rebuild (ort, bundled SQLite, tonic codegen) costs minutes. The host's model cache
is mounted read-only at the path the embedder resolves by default, so the real-model
ONNX tests run here rather than detecting an empty cache and skipping. Requires a
running Docker daemon: the `Stop` hook blocks rather than falling back to the host.

Production code must not use `unwrap()`, `expect()`, `panic!`, or `todo!()` —
these are denied via workspace clippy lints (test code is exempt).
