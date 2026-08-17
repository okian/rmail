//! The output contract: `--format`, the exit-code table, and the one writer
//! every structured surface of this binary goes through (task 42).
//!
//! # `--format json` is an API, not a display choice
//!
//! The moment someone writes `mail search 'from:acme' --format json | jq
//! '.uid'` into a cron job, the key names and value shapes are a contract with
//! that script. This module is where that contract is expressed once, so that
//! a rename in a proto, a reshuffle of a `println!`, or a new field on a
//! generated struct cannot quietly change it.
//!
//! Two kinds of structured output exist here, and the difference is
//! deliberate:
//!
//! - **Curated schemas.** `mail search`/`mail similar` hand-write their JSON
//!   (`search_cli::JsonHit`) precisely so a proto field rename — wire- and
//!   source-compatible for every gRPC client — cannot reshape a shell
//!   pipeline. Those keep their shape; `--format json` selects the same
//!   renderer `--json` always did.
//! - **The response message itself.** For everything else, [`emit_response`]
//!   writes the RPC's *response message* as proto JSON: keys are the proto
//!   field names, produced by the same descriptor-driven codec the MCP adapter
//!   uses (`rmaild::mcp::codec`). That is a stable contract for the same
//!   reason the gRPC surface is one — CLAUDE.md's "breaking changes require a
//!   new version" governs `proto/rmail/v1`, and this output *is* those
//!   messages — and it costs no second schema to drift.
//!
//! A verb that has neither is listed in [`UNSTRUCTURED`], with a reason, and
//! `--format json` on it is a **refusal**, never a table. Printing a human
//! table to a caller who asked for JSON is the one outcome this module exists
//! to prevent: a script that gets an error is fixed in a minute, a script that
//! silently parses a table is wrong for a year.
//! `format::tests::every_cli_verb_declares_how_it_answers_format_json` walks
//! `clap`'s own tree and fails by name for a verb in neither set, so the gap
//! is always a written-down decision rather than an accident.
//!
//! # Streaming
//!
//! `ndjson` is one JSON object per line, flushed as each frame arrives —
//! literally mirroring the gRPC frames, which is what makes `mail notify watch
//! --format ndjson | while read -r line` work at the latency the stream was
//! built to deliver. `json` on a streaming command emits a JSON *array*,
//! written incrementally (`[`, elements, `]`) rather than buffered, so it
//! stays one valid document without giving up the streaming property.
//!
//! # Terminal safety
//!
//! Every string this module writes has usually come from a mailbox or a model,
//! which is to say from an attacker. The table path sanitizes with
//! [`crate::terminal_safe`] as it always has. The JSON path does something
//! slightly different and better: [`SafeFormatter`] escapes anything
//! [`crate::terminal_safe_char`] would drop as a `\uXXXX` sequence, so the
//! bytes on the terminal contain no `ESC`, no bidi override and no invisible
//! tag character, while `serde_json::from_str` still recovers the *original*
//! string exactly. A consumer gets the real subject; a human who `cat`s the
//! same output gets no repainted screen. Sanitizing the value instead would
//! have made `--format json` lossy, which for a data interchange format is a
//! bug rather than a defense.

use std::io::Write;

use anyhow::{Context as _, Result};
use rmail_core::parity::Command;
use serde::Serialize;
use serde_json::Value;

mod exit;
pub(crate) use exit::{Classified, ExitCode};

/// How a command renders its result.
///
/// `table` is the default because `mail` is typed by humans far more often
/// than by scripts, and a default that changed with a pipe would be a
/// different kind of instability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub(crate) enum OutputFormat {
    /// Human-readable columns. What every verb printed before task 42.
    #[default]
    Table,
    /// One JSON document per invocation; a JSON array for a stream.
    Json,
    /// One JSON object per line — one per gRPC frame for a stream.
    Ndjson,
}

impl OutputFormat {
    /// Whether the caller asked for machine-readable output.
    pub(crate) const fn is_structured(self) -> bool {
        !matches!(self, OutputFormat::Table)
    }

    /// The spelling `--format` accepts, for error messages.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            OutputFormat::Table => "table",
            OutputFormat::Json => "json",
            OutputFormat::Ndjson => "ndjson",
        }
    }
}

/// The format this invocation asked for.
///
/// A `OnceLock` rather than a parameter threaded through ninety command
/// functions, because that is what it is: one value, read off argv before any
/// command runs, never written again. [`init`] is called exactly once from
/// `main`; anything that reads it before then — a unit test — gets the
/// default, which is the same answer `mail` gives with no `--format` at all.
static FORMAT: std::sync::OnceLock<OutputFormat> = std::sync::OnceLock::new();

/// Record what `--format` asked for. Called once, from `main`.
pub(crate) fn init(format: OutputFormat) {
    // A second call cannot happen (one `main`, one parse) and must not panic
    // if it somehow did: the first value is the one every emitter already
    // committed to.
    let _ = FORMAT.set(format);
}

/// What `--format` asked for, or [`OutputFormat::Table`] before `main` has
/// parsed anything.
pub(crate) fn current() -> OutputFormat {
    FORMAT.get().copied().unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Terminal-safe JSON
// ---------------------------------------------------------------------------

/// A `serde_json` formatter that `\u`-escapes everything a terminal could act
/// on.
///
/// `serde_json`'s own formatter escapes `U+0000..=U+001F` because RFC 8259
/// requires it, which handles `ESC` — and nothing else. `U+202E` (right-to-left
/// override) and the `U+E0000` tag-character block are ordinary Unicode as far
/// as JSON is concerned and would be written through verbatim, to be *rendered*
/// by whatever terminal the operator piped into. Escaping them here is
/// lossless: the escape decodes back to the same code point, so only the
/// on-the-wire spelling changes.
#[derive(Debug, Clone, Copy, Default)]
struct SafeFormatter;

impl serde_json::ser::Formatter for SafeFormatter {
    fn write_string_fragment<W>(&mut self, writer: &mut W, fragment: &str) -> std::io::Result<()>
    where
        W: ?Sized + Write,
    {
        // `serde_json` hands this the runs *between* the escapes it makes
        // itself, so everything here is already free of `"` , `\` and C0
        // controls; what is left to catch is the printable-but-dangerous set.
        let bytes = fragment.as_bytes();
        let mut plain_from = 0usize;
        for (offset, ch) in fragment.char_indices() {
            if crate::terminal_safe_char(ch) == Some(ch) {
                continue;
            }
            writer.write_all(&bytes[plain_from..offset])?;
            // Non-BMP code points (the U+E0000 tag block among them) have no
            // single `\uXXXX` form; JSON spells them as a surrogate pair.
            let mut units = [0u16; 2];
            for unit in ch.encode_utf16(&mut units) {
                write!(writer, "\\u{unit:04x}")?;
            }
            plain_from = offset + ch.len_utf8();
        }
        writer.write_all(&bytes[plain_from..])
    }
}

/// `value` as a compact single-line JSON string, terminal-safe.
///
/// # Errors
///
/// Propagates a serialization failure (a map with non-string keys, a float
/// that is not finite).
pub(crate) fn to_line<T: Serialize>(value: &T) -> Result<String> {
    let mut out = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(&mut out, SafeFormatter);
    value
        .serialize(&mut ser)
        .context("serializing a JSON output line")?;
    String::from_utf8(out).context("the JSON serializer produced non-UTF-8 output")
}

/// `value` as an indented JSON document, terminal-safe.
///
/// # Errors
///
/// As [`to_line`].
pub(crate) fn to_document<T: Serialize>(value: &T) -> Result<String> {
    let mut out = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::new();
    let mut ser = serde_json::Serializer::with_formatter(
        &mut out,
        SafePretty {
            inner: formatter,
            safe: SafeFormatter,
        },
    );
    value
        .serialize(&mut ser)
        .context("serializing a JSON document")?;
    String::from_utf8(out).context("the JSON serializer produced non-UTF-8 output")
}

/// [`serde_json::ser::PrettyFormatter`]'s indentation with [`SafeFormatter`]'s
/// string escaping.
///
/// `Formatter` has no composition of its own, so the one method that differs
/// is overridden and every other call is delegated. Only `write_string_fragment`
/// is overridden: indentation, separators and the numeric formats are all
/// inherited unchanged, so this cannot drift from `serde_json`'s pretty output
/// in any way other than the escaping it exists to add.
struct SafePretty<'a> {
    inner: serde_json::ser::PrettyFormatter<'a>,
    safe: SafeFormatter,
}

macro_rules! delegate {
    ($( fn $name:ident (&mut self, writer: &mut W $(, $arg:ident : $ty:ty )* ); )*) => {
        $(
            fn $name<W>(&mut self, writer: &mut W $(, $arg: $ty)*) -> std::io::Result<()>
            where
                W: ?Sized + Write,
            {
                self.inner.$name(writer $(, $arg)*)
            }
        )*
    };
}

impl serde_json::ser::Formatter for SafePretty<'_> {
    fn write_string_fragment<W>(&mut self, writer: &mut W, fragment: &str) -> std::io::Result<()>
    where
        W: ?Sized + Write,
    {
        self.safe.write_string_fragment(writer, fragment)
    }

    delegate! {
        fn begin_array(&mut self, writer: &mut W);
        fn end_array(&mut self, writer: &mut W);
        fn begin_array_value(&mut self, writer: &mut W, first: bool);
        fn end_array_value(&mut self, writer: &mut W);
        fn begin_object(&mut self, writer: &mut W);
        fn end_object(&mut self, writer: &mut W);
        fn begin_object_key(&mut self, writer: &mut W, first: bool);
        fn begin_object_value(&mut self, writer: &mut W);
        fn end_object_value(&mut self, writer: &mut W);
    }
}

// ---------------------------------------------------------------------------
// Emitting
// ---------------------------------------------------------------------------

/// One RPC response message, as proto JSON.
///
/// The message name is derived from `command` — the parity registry's RPC path
/// resolved against the compiled descriptor set — rather than written out at
/// the call site, so there is no proto type name in this crate to fall out of
/// date. A row that named a method the descriptor set does not have already
/// fails `rmail_core::parity`'s own drift tests.
///
/// # Errors
///
/// If the descriptor set does not describe `command`'s response message, or
/// the encoded message does not match it — both build-time inconsistencies
/// rather than anything a user can cause.
pub(crate) fn response_json<M: prost::Message>(command: Command, message: &M) -> Result<Value> {
    let catalog = rmaild::mcp::descriptor::catalog()
        .context("indexing the compiled protobuf descriptor set")?;
    let method = catalog
        .methods()
        .iter()
        .find(|m| m.path == command.rpc())
        .with_context(|| {
            format!(
                "{} names {}, which the compiled descriptor set does not declare",
                command.name(),
                command.rpc()
            )
        })?;
    rmaild::mcp::codec::decode(catalog, &method.output_type, &message.encode_to_vec())
        .with_context(|| format!("rendering a {} response as JSON", method.output_type))
}

/// Write `message` as the whole of this command's output, if the caller asked
/// for structured output.
///
/// Returns `true` when it printed — the calling command is then finished and
/// must not also print its table.
///
/// # Errors
///
/// As [`response_json`], plus any failure writing to stdout.
pub(crate) fn emit_response<M: prost::Message>(command: Command, message: &M) -> Result<bool> {
    let format = current();
    if !format.is_structured() {
        return Ok(false);
    }
    let value = response_json(command, message)?;
    let rendered = match format {
        OutputFormat::Json => to_document(&value)?,
        // A single response in `ndjson` is one line: the format is "one JSON
        // value per line", not "one JSON value per stream frame", so a unary
        // RPC is simply a stream of one and needs no special case downstream.
        OutputFormat::Ndjson | OutputFormat::Table => to_line(&value)?,
    };
    let mut out = std::io::stdout().lock();
    writeln!(out, "{rendered}").context("writing structured output to stdout")?;
    out.flush().context("flushing stdout")?;
    Ok(true)
}

/// Whether this invocation wants JSON, counting a verb's own legacy `--json`.
///
/// The per-command `--json` flags predate the global `--format` and are kept
/// as aliases; a verb that consulted only its own flag would print its table
/// to a `--format json` caller, which is the one thing
/// [`is_unstructured`]'s whole design exists to prevent. Every verb listed in
/// [`STRUCTURED`] that still has a `--json` flag goes through here.
pub(crate) fn wants_json(legacy_flag: bool) -> bool {
    legacy_flag || current().is_structured()
}

/// A sequence of JSON values, written as the chosen format spells a sequence.
///
/// `ndjson` is one value per line. `json` is one array, written *incrementally*
/// — `[` goes out with the first element, not after the last — so a caller
/// keeps the property that made a stream worth streaming while still producing
/// one valid document. Buffering the whole sequence to serialize a `Vec` would
/// have been three lines shorter and would have thrown away the latency the
/// pipeline exists to deliver.
pub(crate) struct JsonSeq {
    format: OutputFormat,
    /// Whether the opening `[` has been written, which is also what says
    /// whether the next element needs a leading comma.
    started: bool,
}

impl JsonSeq {
    /// Open a sequence in the format this invocation asked for.
    pub(crate) fn open() -> Self {
        Self {
            format: current(),
            started: false,
        }
    }

    /// Open a newline-delimited sequence regardless of `--format`.
    ///
    /// The per-command `--json` flags predate this module and mean exactly
    /// "one JSON object per line". They are kept as aliases rather than
    /// removed — scripts already use them — and they keep meaning what they
    /// meant, which is what this constructor is for. `--format ndjson` is the
    /// spelling to document.
    pub(crate) const fn ndjson() -> Self {
        Self {
            format: OutputFormat::Ndjson,
            started: false,
        }
    }

    /// Whether anything written to this sequence will be printed.
    pub(crate) const fn is_structured(&self) -> bool {
        self.format.is_structured()
    }

    /// Write one element. A no-op in `table` mode.
    ///
    /// # Errors
    ///
    /// Serialization failure, or any failure writing to stdout.
    pub(crate) fn write<T: Serialize>(&mut self, value: &T) -> Result<()> {
        if !self.format.is_structured() {
            return Ok(());
        }
        let line = to_line(value)?;
        let mut out = std::io::stdout().lock();
        match self.format {
            OutputFormat::Ndjson => writeln!(out, "{line}"),
            OutputFormat::Json => {
                let separator = if self.started { ",\n  " } else { "[\n  " };
                self.started = true;
                write!(out, "{separator}{line}")
            }
            OutputFormat::Table => Ok(()),
        }
        .context("writing a structured record to stdout")?;
        // Per element, not per sequence: the point is that a consumer sees a
        // frame when the daemon sent it, and a pipe's buffered stdout would
        // hold it back until the process exited.
        out.flush().context("flushing stdout")
    }

    /// Close the document.
    ///
    /// Only `json` has anything to close, and an empty sequence still has to
    /// produce `[]` rather than nothing at all — a consumer running
    /// `mail … --format json | jq 'length'` on a quiet mailbox must get `0`,
    /// not a parse error.
    ///
    /// # Errors
    ///
    /// Any failure writing to stdout.
    pub(crate) fn finish(self) -> Result<()> {
        if self.format != OutputFormat::Json {
            return Ok(());
        }
        let mut out = std::io::stdout().lock();
        if self.started {
            writeln!(out, "\n]")
        } else {
            writeln!(out, "[]")
        }
        .context("closing the JSON array")?;
        out.flush().context("flushing stdout")
    }
}

/// A [`JsonSeq`] whose elements are one RPC's response frames.
///
/// Inert in `table` mode — [`Frames::emit`] answers `false` and the command
/// renders its own row — which is what lets a streaming command carry one loop
/// body for all three formats instead of three.
pub(crate) struct Frames {
    seq: JsonSeq,
    command: Command,
}

impl Frames {
    /// Open a sink for `command`'s response stream.
    pub(crate) fn open(command: Command) -> Self {
        Self {
            seq: JsonSeq::open(),
            command,
        }
    }

    /// Write one frame, if the caller asked for structured output.
    ///
    /// Returns `true` when it printed, meaning the caller should not also
    /// print its own row for this frame.
    ///
    /// # Errors
    ///
    /// As [`response_json`], plus any failure writing to stdout.
    pub(crate) fn emit<M: prost::Message>(&mut self, message: &M) -> Result<bool> {
        if !self.seq.is_structured() {
            return Ok(false);
        }
        let value = response_json(self.command, message)?;
        self.seq.write(&value)?;
        Ok(true)
    }

    /// Close the document.
    ///
    /// # Errors
    ///
    /// Any failure writing to stdout.
    pub(crate) fn finish(self) -> Result<()> {
        self.seq.finish()
    }
}

// ---------------------------------------------------------------------------
// The verbs that have no structured form, and why
// ---------------------------------------------------------------------------

/// `mail` verbs that answer `--format json` with a refusal, and the reason.
///
/// The escape hatch on "a global `--format` on every command", written by hand
/// on purpose and in the shape `rmail_core::parity::LOCAL_CLI` established: a
/// verb that reaches this list is a decision someone made in a diff a reviewer
/// read, and a verb that reaches *neither* list fails the suite by name.
///
/// Three arguments are represented, and it is worth keeping them distinct:
///
/// - **Not a report.** `tui`, `mcp serve`, `daemon start`/`stop`, `agent run`
///   and the `keys`/`hook add` file editors do something rather than answer
///   something. A JSON rendering of "the TUI ran" is not data anyone can use.
/// - **Already a file format.** `mail export` writes an archive, and
///   `--archive-format json` *is* its JSON form. The global flag has nothing
///   left to mean for it.
/// - **A human report with no schema yet.** The rest. Their response messages
///   are perfectly renderable — `mail api call <Method> '<json>'` will print
///   exactly that, today, for any RPC in the descriptor set — but a *curated*
///   schema is a promise about key names, and promising one per verb is work
///   that belongs with the verb, not with this module. Refusing is the honest
///   answer until it is done; printing the table would not be.
pub(crate) const UNSTRUCTURED: &[(&str, &str)] = &[
    (
        "tui",
        "runs a full-screen terminal UI; there is no document to emit",
    ),
    (
        "mcp serve",
        "serves JSON-RPC on stdout — a second structured stream on the same fd is the one thing \
         it must not do",
    ),
    (
        "keys list",
        "reads keys.toml; `mail api call ConfigService.GetKeymap '{}'` is the machine-readable form",
    ),
    ("keys set", "rewrites keys.toml and reports what it wrote"),
    ("keys unset", "rewrites keys.toml and reports what it wrote"),
    (
        "keys actions",
        "prints the compiled-in action registry, which is a listing of the binary and not of any \
         mailbox",
    ),
    (
        "hook add",
        "appends a block to the operator's own config file and echoes it back",
    ),
    (
        "daemon start",
        "starts a process; `mail daemon status --format json` is the machine-readable question",
    ),
    (
        "daemon stop",
        "stops a process; `mail daemon status --format json` is the machine-readable question",
    ),
    (
        "account login",
        "an interactive browser flow whose output is a URL for a human to open",
    ),
    (
        "export",
        "writes an archive to a path, and `--archive-format json` is its JSON form; a second, \
         global JSON would have nothing to mean (and, spelled `--format`, used to overwrite the \
         archive format — see `export_cli`'s own docs)",
    ),
];

/// `mail` verbs that render `--format json` and `--format ndjson`.
///
/// The other half of the declaration
/// `every_cli_verb_declares_how_it_answers_format_json` demands. A path here
/// is a promise that the verb writes JSON — through [`emit_response`],
/// [`Frames`], or a curated schema of its own — and never falls through to its
/// table.
pub(crate) const STRUCTURED: &[&str] = &[
    // Curated schemas, predating this task and kept exactly as they were:
    // `--json` is now the alias and `--format json` the spelling.
    "search",
    "similar",
    "find",
    "search eval",
    "search train",
    "search models",
    "ai summary",
    // The response message itself, via `emit_response`/`Frames`.
    "ping",
    "notify score",
    "notify watch",
    "token list",
    "ai status",
    "api ping",
    "api reflect",
    "api call",
    "daemon status",
];

/// The reason shared by every verb in [`NO_CURATED_SCHEMA`].
const NO_CURATED_SCHEMA_REASON: &str =
    "its response has no curated JSON schema yet, and this build will not hand a caller who \
     asked for JSON a human table instead";

/// `mail` verbs whose RPC response is perfectly renderable and simply has no
/// curated schema written for it yet.
///
/// Separated from [`UNSTRUCTURED`] because the two are different statements. A
/// verb there is a *decision* — there is nothing to serialize, and there never
/// will be. A verb here is *unfinished work*, and the honest thing is to say
/// so in one place rather than repeat a sentence eighty times.
///
/// What it does **not** mean is that the data is unreachable. Every one of
/// these is backed by a capability row (`rmail_core::parity` enforces that),
/// so its response is one command away today:
///
/// ```text
/// mail api call SyncService.SyncFolder '{"account_id":1}'
/// ```
///
/// prints exactly the response message, as proto JSON, with the proto's own
/// field names — the same rendering [`emit_response`] gives the verbs that
/// have been converted. What is missing is not the capability but the
/// *curation*: deciding, per verb, whether the response message is the shape a
/// script should depend on or whether a hand-written projection of it is (the
/// argument `search_cli::JsonHit` makes for `mail search`). That decision
/// belongs with each verb, and making it eighty times in one commit would be
/// making it carelessly eighty times.
pub(crate) const NO_CURATED_SCHEMA: &[&str] = &[
    "sync",
    "list",
    "account add",
    "account refresh",
    "token create",
    "token revoke",
    "search rollback",
    "folder new",
    "folder list",
    "folder members",
    "folder eval",
    "folder rm",
    "ai process",
    "ai reply",
    "ai retry",
    "ai pause",
    "ai resume",
    "ai cost",
    "ai budget set",
    "ai budget status",
    "ai provider set",
    "ai provider status",
    "ai scan-injection",
    "ask",
    "note add",
    "note edit",
    "note rm",
    "notes",
    "agent run",
    "agent log",
    "hook list",
    "hook test",
    "webhook add",
    "webhook list",
    "webhook rm",
    "webhook enable",
    "webhook disable",
    "webhook deliveries",
    "webhook replay",
    "forward",
    "index status",
    "index run",
    "index start",
    "index stop",
    "index reindex",
    "index rebuild",
    "index verify",
    "index gc",
    "index embed",
    "entities",
    "tag",
    "tag-bulk",
    "untag",
    "tags",
    "tags create",
    "suggest-tags",
    "accept-tags",
    "reject-tags",
    "tag-rules list",
    "tag-rules set",
    "reply",
    "draft rewrite",
    "draft revisions",
    "draft revert",
    "send",
    "undo",
    "outbox",
    "outbox show",
    "outbox cancel",
    "outbox reschedule",
    "outbox edit",
    "outbox retry",
    "outbox send-now",
    "outbox suggest",
    "followup add",
    "followup list",
    "followup dismiss",
    "stats response-time",
    "stats ask",
    "contact",
    "subs",
    "digest",
    "attach tables",
    "attach invoice",
    "invoices",
    "extract events",
    "extract tasks",
    "extract data",
    "links",
];

/// Whether `path` (a space-separated subcommand path, no leading `mail`) is
/// declared as having no structured form, and why.
pub(crate) fn is_unstructured(path: &str) -> Option<&'static str> {
    if let Some((_, why)) = UNSTRUCTURED.iter().find(|(verb, _)| *verb == path) {
        return Some(why);
    }
    NO_CURATED_SCHEMA
        .contains(&path)
        .then_some(NO_CURATED_SCHEMA_REASON)
}

#[cfg(test)]
mod tests;
