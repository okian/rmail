//! `mail reply` / `mail draft` — the CLI half of AI reply drafting and the
//! tone/length rewrite (task 62), over `ComposeService`.
//!
//! # Why `--ai` is a required flag rather than an optional one
//!
//! prd.md spells this verb `mail reply <id> --ai "…"`, and the flag is
//! `required = true` so that spelling is the only one. There is no non-AI
//! `mail reply` today — a plain reply draft needs a `From` identity the
//! client would have to be told, which `DraftReply` derives server-side — so
//! the alternative was a verb that silently called a model whenever it was
//! typed. A flag that must be present is how a paid, network-crossing action
//! stays something the user asked for in as many words.
//!
//! # What the terminal shows
//!
//! The reply streams to stdout as it is written, which is the point of a
//! streaming RPC. Everything that is *not* the reply — what was read, which
//! model, and the id of the draft that was staged — goes to stderr, so
//! `mail reply 42 --ai "yes" > body.txt` yields exactly the body and nothing
//! else. `--quiet` drops the stderr commentary too.
//!
//! Nothing here can send. `DraftReply` stages a draft and stops; putting it
//! on the wire is `mail send`, which is a different verb with a different
//! scope and its own pre-send guardian.

use std::io::Write as _;
use std::path::Path;

use anyhow::{bail, Context, Result};
use clap::Subcommand;
use rmail_proto::v1::compose_service_client::ComposeServiceClient;
use rmail_proto::v1::{
    draft_reply_event, Draft, DraftReplyRequest, DraftRevision, ListDraftRevisionsRequest,
    RewriteDraftRequest, RewriteLength, RewriteTone, SelectDraftRevisionRequest,
};
use tokio_stream::StreamExt;

/// `mail reply <message_id> --ai "<intent>"`.
#[derive(Debug, clap::Args)]
pub struct ReplyArgs {
    /// The message to reply to.
    pub message_id: i64,
    /// A short statement of what the reply should say ("yes, but push to
    /// Tuesday"). Optional: with none, the shortest reply that moves the
    /// thread forward is written.
    #[arg(default_value = "")]
    pub intent: String,
    /// Draft the reply with Claude. Required — see this module's own docs on
    /// why there is no implicit form.
    #[arg(long, required = true)]
    pub ai: bool,
    /// Address everyone the message addressed, not only its author. Your own
    /// addresses are always dropped.
    #[arg(long)]
    pub reply_all: bool,
    /// Print only the drafted body: no context line, no draft id.
    #[arg(long)]
    pub quiet: bool,
}

/// `mail draft <action>`.
#[derive(Debug, Subcommand)]
pub enum DraftAction {
    /// Rewrite a draft to a target tone and/or length
    /// (`ComposeService.RewriteDraft`).
    ///
    /// The result is stored as a new revision, not an edit: the text you had
    /// before the first rewrite is kept as revision 0, and
    /// `mail draft revert <id>` brings it back.
    Rewrite {
        /// The draft to rewrite.
        draft_id: i64,
        /// Target register: formal, casual, warmer, firmer, mirror, as_is.
        #[arg(long)]
        tone: Option<String>,
        /// Cut length, keeping every fact.
        #[arg(long)]
        shorter: bool,
        /// Expand: more context, no new facts.
        #[arg(long)]
        longer: bool,
        /// Extra free-form instruction ("drop the apology").
        #[arg(long, default_value = "")]
        instruction: String,
    },
    /// List a draft's stored revisions, oldest first
    /// (`ComposeService.ListDraftRevisions`).
    Revisions {
        /// The draft.
        draft_id: i64,
    },
    /// Point a draft at one of its revisions — the cycle and the revert
    /// (`ComposeService.SelectDraftRevision`).
    ///
    /// Non-destructive in both directions: whatever the draft says now is
    /// written back onto the revision it is leaving, so an edit you made by
    /// hand after a rewrite is still there when you cycle back to it.
    Revert {
        /// The draft.
        draft_id: i64,
        /// Which revision to make active. 0 (the default) is what you
        /// originally typed.
        #[arg(long, default_value_t = 0)]
        seq: i64,
    },
}

async fn client(socket: &Path) -> Result<ComposeServiceClient<crate::client::Client>> {
    let channel = crate::client::connect(socket).await?;
    Ok(ComposeServiceClient::new(channel))
}

/// `mail reply` — stream a drafted reply and report the draft it staged.
pub async fn reply(socket: &Path, args: ReplyArgs) -> Result<()> {
    let mut stream = client(socket)
        .await?
        .draft_reply(DraftReplyRequest {
            message_id: args.message_id,
            intent: args.intent,
            reply_all: args.reply_all,
        })
        .await
        .context("DraftReply RPC failed")?
        .into_inner();

    let mut staged: Option<Draft> = None;
    let mut wrote_body = false;
    while let Some(event) = stream.next().await {
        let Some(event) = event.context("DraftReply stream failed")?.event else {
            // A frame with no variant set: a newer server sending something
            // this client has no name for. Skipped rather than treated as the
            // end of the stream.
            continue;
        };
        match event {
            draft_reply_event::Event::Context(context) if !args.quiet => {
                eprintln!(
                    "reading {} thread message(s), {} past repl(y|ies){} — {}",
                    context.thread_messages,
                    context.voice_samples,
                    if context.withheld_by_policy > 0 {
                        format!(", {} withheld by ai.policy", context.withheld_by_policy)
                    } else {
                        String::new()
                    },
                    context.model
                );
            }
            draft_reply_event::Event::Context(_) => {}
            draft_reply_event::Event::Token(token) => {
                print!("{token}");
                // Flushed per token: the whole point of the stream is that a
                // person watches it arrive, and stdout to a pipe or a file is
                // block-buffered by default.
                std::io::stdout().flush().ok();
                wrote_body = true;
            }
            draft_reply_event::Event::Draft(draft) => staged = Some(draft),
            draft_reply_event::Event::Usage(_) => {}
            draft_reply_event::Event::Done(_) => {}
        }
    }
    if wrote_body {
        println!();
    }

    match staged {
        Some(draft) => {
            if !args.quiet {
                eprintln!(
                    "staged draft {} to {} — nothing has been sent (`mail send --draft {}` sends it)",
                    draft.id,
                    draft
                        .to
                        .iter()
                        .map(|a| a.address.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                    draft.id
                );
            }
            Ok(())
        }
        // The stream ended without staging anything and without an error
        // frame. Reported rather than exiting 0: a caller redirecting stdout
        // would otherwise get a body with no draft behind it and no sign.
        None => bail!("the reply stream ended without staging a draft"),
    }
}

/// `mail draft <action>`.
pub async fn dispatch(socket: &Path, action: DraftAction) -> Result<()> {
    match action {
        DraftAction::Rewrite {
            draft_id,
            tone,
            shorter,
            longer,
            instruction,
        } => rewrite(socket, draft_id, tone, shorter, longer, instruction).await,
        DraftAction::Revisions { draft_id } => revisions(socket, draft_id).await,
        DraftAction::Revert { draft_id, seq } => revert(socket, draft_id, seq).await,
    }
}

async fn rewrite(
    socket: &Path,
    draft_id: i64,
    tone: Option<String>,
    shorter: bool,
    longer: bool,
    instruction: String,
) -> Result<()> {
    if shorter && longer {
        bail!("--shorter and --longer ask for opposite things; pick one");
    }
    let tone = match tone.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        None => RewriteTone::AsIs,
        Some(name) => parse_tone(name)?,
    };
    let length = if shorter {
        RewriteLength::Shorter
    } else if longer {
        RewriteLength::Longer
    } else {
        RewriteLength::AsIs
    };
    // Refused here as well as server-side: a round trip to be told the
    // command asked for nothing is a round trip that did not need making.
    if tone == RewriteTone::AsIs && length == RewriteLength::AsIs && instruction.trim().is_empty() {
        bail!("nothing to do: give --tone, --shorter/--longer, or --instruction");
    }

    let revision = client(socket)
        .await?
        .rewrite_draft(RewriteDraftRequest {
            draft_id,
            tone: tone as i32,
            length: length as i32,
            instruction,
        })
        .await
        .context("RewriteDraft RPC failed")?
        .into_inner();

    println!("{}", revision.body_text);
    eprintln!(
        "revision {} ({}) is now active on draft {} — `mail draft revert {}` undoes it",
        revision.seq, revision.label, draft_id, draft_id
    );
    Ok(())
}

async fn revisions(socket: &Path, draft_id: i64) -> Result<()> {
    let response = client(socket)
        .await?
        .list_draft_revisions(ListDraftRevisionsRequest { draft_id })
        .await
        .context("ListDraftRevisions RPC failed")?
        .into_inner();

    if response.revisions.is_empty() {
        println!("draft {draft_id} has no revisions (it has never been rewritten)");
        return Ok(());
    }
    for revision in &response.revisions {
        print_revision(revision);
    }
    Ok(())
}

async fn revert(socket: &Path, draft_id: i64, seq: i64) -> Result<()> {
    let draft = client(socket)
        .await?
        .select_draft_revision(SelectDraftRevisionRequest { draft_id, seq })
        .await
        .context("SelectDraftRevision RPC failed")?
        .into_inner();
    println!("{}", draft.body_text);
    eprintln!("draft {draft_id} now holds revision {seq}");
    Ok(())
}

fn print_revision(revision: &DraftRevision) {
    let marker = if revision.active { "*" } else { " " };
    let model = revision.model.as_deref().unwrap_or("(yours)");
    println!(
        "{marker} {:>3}  {:<28} {model}",
        revision.seq,
        truncate(&revision.label, 28)
    );
    for line in revision.body_text.lines().take(3) {
        println!("       {}", truncate(line, 96));
    }
}

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let head: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{head}…")
}

/// Parse a `--tone` value.
///
/// The vocabulary is the wire enum's, spelled the way
/// `rmail_core::compose::reply::Tone::as_str` spells it, so a name that works
/// here works in a config file and in an MCP call too.
fn parse_tone(name: &str) -> Result<RewriteTone> {
    match name.to_ascii_lowercase().as_str() {
        "as_is" | "as-is" => Ok(RewriteTone::AsIs),
        "formal" => Ok(RewriteTone::Formal),
        "casual" => Ok(RewriteTone::Casual),
        "warmer" => Ok(RewriteTone::Warmer),
        "firmer" => Ok(RewriteTone::Firmer),
        "mirror" => Ok(RewriteTone::Mirror),
        other => bail!(
            "unknown tone {other:?}; expected one of: formal, casual, warmer, firmer, mirror, as_is"
        ),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::*;

    #[test]
    fn every_tone_spelling_the_help_advertises_parses() {
        for (name, expected) in [
            ("formal", RewriteTone::Formal),
            ("FORMAL", RewriteTone::Formal),
            ("casual", RewriteTone::Casual),
            ("warmer", RewriteTone::Warmer),
            ("firmer", RewriteTone::Firmer),
            ("mirror", RewriteTone::Mirror),
            ("as_is", RewriteTone::AsIs),
            ("as-is", RewriteTone::AsIs),
        ] {
            assert_eq!(parse_tone(name).unwrap(), expected, "tone {name}");
        }
        assert!(parse_tone("shouty").is_err());
    }

    #[test]
    fn the_cli_tone_vocabulary_matches_the_domains() {
        // Two spellings of the same vocabulary would drift the first time one
        // gained a tone; this fails by name when they do.
        for tone in rmail_core::compose::reply::Tone::ALL {
            assert!(
                parse_tone(tone.as_str()).is_ok(),
                "`mail draft rewrite --tone {}` is a tone the domain has and the CLI cannot spell",
                tone.as_str()
            );
        }
    }

    #[test]
    fn a_long_label_is_truncated_on_a_character_boundary() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("é".repeat(10).as_str(), 4), "ééé…");
    }
}
