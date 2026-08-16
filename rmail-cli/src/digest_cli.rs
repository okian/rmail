//! `mail digest` — the periodic AI briefing, on demand
//! (`AnalyticsService.GenerateDigest`, task 70, prd.md feature 57).
//!
//! # What "on demand" costs
//!
//! A digest is one Sonnet call over a window of mail, and reuse is keyed on
//! the *exact* window — so whether running this twice costs twice depends on
//! which form you use, and the difference is worth stating plainly:
//!
//! - **`mail digest`** (no flags) asks for the last completed period, which is
//!   a fixed window. Running it twice is free the second time (`cached` in the
//!   output), and if the scheduled job has already briefed that period this
//!   just prints it. "Print yesterday's briefing" and "produce it" are the
//!   same command.
//! - **`mail digest --since 7d`** asks for a window ending *now*, which is a
//!   different window every second. It is therefore a fresh call every time,
//!   by construction. That is the honest reading of an explicit request — you
//!   asked about the last seven days as of now — but it is a model call, so it
//!   is not the form to put in a loop.
//!
//! `--force` regenerates regardless, and the output says when a briefing came
//! from the store.
//!
//! # `--since` takes a duration, not a timestamp
//!
//! `--since 7d` reads better than a unix second and is what every other
//! duration-shaped flag in this CLI accepts — the same reasoning
//! `stats_cli`'s own module docs give, and the same parser, so `7d`, `12h`
//! and `90s` mean here exactly what they mean there.
//!
//! With no flags at all the daemon briefs the most recently *completed*
//! period on its configured cadence — the same window the scheduled job would
//! have produced. That is what makes `mail digest` and the timer agree rather
//! than each producing a differently-aligned week.
//!
//! # The `--json` schema
//!
//! One object. Hand-written rather than derived from the wire types, for the
//! reason `search_cli`'s module docs give: a proto field rename must not
//! silently reshape a documented CLI contract.
//!
//! ```json
//! {
//!   "digest_id": 12,
//!   "since": 1700000000, "until": 1700086400,
//!   "account_id": 0, "generated_at": 1700086500,
//!   "model": "claude-sonnet-5", "cached": false, "empty": false,
//!   "considered": 41, "packed": 30, "withheld_by_policy": 2, "clusters": 9,
//!   "markdown": "## Needs reply\n- ...",
//!   "sections": [{"id": "needs_reply", "heading": "Needs reply",
//!                 "lines": [{"text": "...", "message_ids": [42]}]}],
//!   "sources": [{"label": 1, "message_id": 42, "message_uid": 991,
//!                "account_id": 1, "mailbox": "INBOX", "subject": "...",
//!                "from_addr": "a@b.example", "date": 1700000100,
//!                "cited": true}]
//! }
//! ```

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use clap::Args;
use rmail_proto::v1::analytics_service_client::AnalyticsServiceClient;
use rmail_proto::v1::{GenerateDigestRequest, GenerateDigestResponse};
use serde::Serialize;

/// `mail digest [flags]`.
#[derive(Debug, Args)]
pub struct DigestArgs {
    /// How far back to brief, e.g. `7d`, `24h`. With no `--since` and no
    /// `--until` the daemon briefs the last completed period on its own
    /// cadence.
    #[arg(long)]
    since: Option<String>,
    /// End the window here instead of now, as unix seconds.
    #[arg(long)]
    until: Option<i64>,
    /// Restrict to one account. The default covers every configured account.
    #[arg(long)]
    account: Option<i64>,
    /// Regenerate even if this window has already been briefed. Costs another
    /// model call and replaces the stored briefing.
    #[arg(long)]
    force: bool,
    /// One JSON document instead of the markdown briefing.
    #[arg(long)]
    json: bool,
}

/// Run `mail digest`.
///
/// # Errors
///
/// Anything that stops the command completing: an unparseable duration, no
/// daemon, a failed RPC, an unwritable stdout.
pub async fn run(socket: &Path, args: DigestArgs) -> Result<()> {
    let since = args
        .since
        .as_deref()
        .map(parse_duration)
        .transpose()
        .context("--since")?;
    // A relative `--since` becomes an absolute bound here rather than at the
    // daemon, for the reason `stats_cli` gives: the response has to name the
    // window it briefed, and a relative one would mean something different by
    // the time it was rendered.
    let since_abs = since.map_or(0, |seconds| now().saturating_sub(seconds));

    let channel = rmail_core::connect_uds(socket)
        .await
        .with_context(|| format!("connecting to rmaild at {}", socket.display()))?;
    let mut client = AnalyticsServiceClient::new(channel);

    let digest = client
        .generate_digest(GenerateDigestRequest {
            account_id: args.account.unwrap_or(0),
            since: since_abs,
            until: args.until.unwrap_or(0),
            force: args.force,
        })
        .await
        .context("GenerateDigest RPC failed")?
        .into_inner();

    let mut out = std::io::stdout().lock();
    if args.json {
        let line = serde_json::to_string(&to_json(&digest))?;
        writeln!(out, "{line}")?;
        return Ok(());
    }
    print_digest(&mut out, &digest)
}

fn print_digest(out: &mut impl Write, digest: &GenerateDigestResponse) -> Result<()> {
    writeln!(
        out,
        "digest #{}  {} .. {}",
        digest.digest_id, digest.since, digest.until
    )?;
    if digest.empty {
        // Said plainly rather than left for the reader to infer from five
        // `_none_` sections: "nothing arrived" and "the briefing failed" look
        // identical in the markdown and are very different facts.
        writeln!(
            out,
            "note        no mail in this window; nothing was sent to a model"
        )?;
    } else {
        writeln!(
            out,
            "sources     {} of {} considered ({} withheld by ai.policy) in {} cluster(s)",
            digest.packed, digest.considered, digest.withheld_by_policy, digest.clusters
        )?;
        writeln!(
            out,
            "model       {}{}",
            if digest.model.is_empty() {
                "(none)"
            } else {
                &digest.model
            },
            if digest.cached {
                "  (stored briefing; --force to regenerate)"
            } else {
                ""
            }
        )?;
    }
    writeln!(out)?;
    writeln!(out, "{}", digest.markdown)?;
    if !digest.sources.is_empty() {
        writeln!(out)?;
        writeln!(out, "sources")?;
        for source in &digest.sources {
            writeln!(
                out,
                "  msg:{:<8} {:<24} {}",
                source.message_id,
                truncate(&source.from_addr, 24),
                truncate(&source.subject, 60)
            )?;
        }
    }
    Ok(())
}

/// `text`, cut to at most `max` characters. By `char`, not by byte: slicing a
/// UTF-8 string at an arbitrary byte offset panics, and mail is full of
/// multi-byte text.
fn truncate(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((cut, _)) => format!("{}…", text.get(..cut).unwrap_or_default()),
        None => text.to_owned(),
    }
}

#[derive(Serialize)]
struct JsonDigest<'a> {
    digest_id: i64,
    since: i64,
    until: i64,
    account_id: i64,
    generated_at: i64,
    model: &'a str,
    cached: bool,
    empty: bool,
    considered: u64,
    packed: u64,
    withheld_by_policy: u64,
    clusters: u64,
    markdown: &'a str,
    sections: Vec<JsonSection<'a>>,
    sources: Vec<JsonSource<'a>>,
}

#[derive(Serialize)]
struct JsonSection<'a> {
    id: &'a str,
    heading: &'a str,
    lines: Vec<JsonLine<'a>>,
}

#[derive(Serialize)]
struct JsonLine<'a> {
    text: &'a str,
    message_ids: &'a [i64],
}

#[derive(Serialize)]
struct JsonSource<'a> {
    label: u32,
    message_id: i64,
    message_uid: i64,
    account_id: i64,
    mailbox: &'a str,
    subject: &'a str,
    from_addr: &'a str,
    date: i64,
    cited: bool,
}

fn to_json(digest: &GenerateDigestResponse) -> JsonDigest<'_> {
    JsonDigest {
        digest_id: digest.digest_id,
        since: digest.since,
        until: digest.until,
        account_id: digest.account_id,
        generated_at: digest.generated_at,
        model: &digest.model,
        cached: digest.cached,
        empty: digest.empty,
        considered: digest.considered,
        packed: digest.packed,
        withheld_by_policy: digest.withheld_by_policy,
        clusters: digest.clusters,
        markdown: &digest.markdown,
        sections: digest
            .sections
            .iter()
            .map(|section| JsonSection {
                id: &section.id,
                heading: &section.heading,
                lines: section
                    .lines
                    .iter()
                    .map(|line| JsonLine {
                        text: &line.text,
                        message_ids: &line.message_ids,
                    })
                    .collect(),
            })
            .collect(),
        sources: digest
            .sources
            .iter()
            .map(|source| JsonSource {
                label: source.label,
                message_id: source.message_id,
                message_uid: source.message_uid,
                account_id: source.account_id,
                mailbox: &source.mailbox,
                subject: &source.subject,
                from_addr: &source.from_addr,
                date: source.date,
                cited: source.cited,
            })
            .collect(),
    }
}

/// `30d` / `12h` / `90s` in seconds. The same grammar `mail stats --since`
/// accepts; duplicated rather than shared because these are two independent
/// CLI contracts and a change to one must not silently change the other.
fn parse_duration(value: &str) -> Result<i64> {
    let trimmed = value.trim();
    let (digits, multiplier) = match trimmed.chars().last() {
        Some('s') => (&trimmed[..trimmed.len() - 1], 1),
        Some('m') => (&trimmed[..trimmed.len() - 1], 60),
        Some('h') => (&trimmed[..trimmed.len() - 1], 3_600),
        Some('d') => (&trimmed[..trimmed.len() - 1], 86_400),
        Some('w') => (&trimmed[..trimmed.len() - 1], 7 * 86_400),
        _ => (trimmed, 1),
    };
    let count: i64 = digits
        .trim()
        .parse()
        .with_context(|| format!("`{value}` is not a duration like `7d`, `12h` or `90s`"))?;
    if count <= 0 {
        anyhow::bail!("`{value}` must be a positive duration");
    }
    count
        .checked_mul(multiplier)
        .with_context(|| format!("`{value}` is too large to express in seconds"))
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_parse_the_documented_grammar() {
        assert_eq!(parse_duration("7d").unwrap(), 7 * 86_400);
        assert_eq!(parse_duration("12h").unwrap(), 12 * 3_600);
        assert_eq!(parse_duration("90s").unwrap(), 90);
        assert!(parse_duration("0d").is_err());
        assert!(parse_duration("soon").is_err());
    }

    #[test]
    fn truncation_never_splits_a_multibyte_character() {
        // A byte-offset slice here would panic; this is the regression the
        // `char_indices` form exists for.
        let text = "päivitys —— aikataulu";
        let cut = truncate(text, 3);
        assert!(text.starts_with(cut.trim_end_matches('…')));
    }
}
