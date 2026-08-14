//! The `AiSafetyService` gRPC implementation: the prompt-injection shield's
//! read-and-confirm surface (task 77, prd.md #43).
//!
//! # Neither RPC here is what protects anything
//!
//! The shield's actual control is structural and lives entirely in
//! [`rmail_core::ai::injection`]: untrusted mail is wrapped in labelled
//! delimiters and every model-facing system prompt says that what is inside
//! them is data, never instruction. That happens on the request-building
//! path with no RPC involved and no way for a client to turn it off.
//!
//! What this service exposes is the shield's *second* half — the detector's
//! findings — for the two things a pattern scan is genuinely good for:
//! letting a user see that a message tried something, and letting a human
//! release a rule action the shield withheld because of it.
//!
//! # ScanInjection makes no model call, and that shapes its scope
//!
//! It runs a local regex scan over text the daemon already holds. No
//! provider, no token, no cost — which is why its row in
//! [`crate::auth::methods`] is `mail.read` and not `ai.invoke`: what a
//! caller gets out of it is quoted message content, so it needs the scope
//! that governs reading mail, and nothing more. `ConfirmInjection` is the
//! opposite case and sits behind `automation` + `mail.write`, because its
//! entire effect is to let a rule move, archive, label, hook and draft on a
//! message the shield had refused to act on — exactly the pair
//! `RuleService/EvaluateRules` already requires for the same authority.
//!
//! # Confirming is not something the pipeline can do for itself
//!
//! Nothing in [`rmail_core::ai`] ever calls `set_confirmed`. A confirmation
//! that a machine could grant itself would be a field, not a control, and
//! the withhold it releases exists precisely because the machine's judgement
//! is what is in doubt.
//
// `tonic::Status` is intentionally the error type throughout a gRPC service
// boundary; its size makes `result_large_err` fire on every
// `Result<_, Status>` helper, so the lint is allowed for this module — the
// same allowance `ai_policy_service.rs`/`audit_service.rs` carry.
#![allow(clippy::result_large_err)]

use rmail_core::ai::injection::store::{self, Flag};
use rmail_core::config::{AiInjection, AiPrivacy};
use rmail_core::storage::Database;
use rmail_proto::v1::ai_safety_service_server::AiSafetyService;
use rmail_proto::v1::{
    ConfirmInjectionRequest, ConfirmInjectionResponse, InjectionDetection, InjectionSeverity,
    ScanInjectionRequest, ScanInjectionResponse,
};
use tonic::{Request, Response, Status};

/// The `AiSafetyService` handler.
#[derive(Debug, Clone)]
pub struct AiSafetyApi {
    db: Database,
    /// `ai.privacy` — so the scan assembles content under exactly the
    /// bounds (`max_body_chars`, `strip_attachments`) a real pass would,
    /// and a report never describes text no request would have carried.
    privacy: AiPrivacy,
    /// `ai.injection` — the detector switch and the threshold
    /// `actions_withheld` is derived against.
    injection: AiInjection,
}

impl AiSafetyApi {
    /// Build a handler over `db`, scanning under `privacy` and reporting
    /// against `injection`'s configured threshold.
    #[must_use]
    pub fn new(db: Database, privacy: AiPrivacy, injection: AiInjection) -> Self {
        Self {
            db,
            privacy,
            injection,
        }
    }
}

#[tonic::async_trait]
impl AiSafetyService for AiSafetyApi {
    #[tracing::instrument(skip(self, request))]
    async fn scan_injection(
        &self,
        request: Request<ScanInjectionRequest>,
    ) -> Result<Response<ScanInjectionResponse>, Status> {
        let message_id = validate_id(request.into_inner().message_id)?;
        let flag = store::scan_message(&self.db, message_id, &self.privacy, &self.injection)
            .await
            .map_err(Status::from)?;
        Ok(Response::new(self.to_proto(message_id, flag.as_ref())))
    }

    #[tracing::instrument(skip(self, request))]
    async fn confirm_injection(
        &self,
        request: Request<ConfirmInjectionRequest>,
    ) -> Result<Response<ConfirmInjectionResponse>, Status> {
        let request = request.into_inner();
        let message_id = validate_id(request.message_id)?;
        let flag = store::set_confirmed(&self.db, message_id, request.confirmed)
            .await
            .map_err(Status::from)?;
        Ok(Response::new(ConfirmInjectionResponse {
            flag: Some(self.to_proto(message_id, Some(&flag))),
        }))
    }
}

impl AiSafetyApi {
    /// One flag on the wire. `None` — a clean scan — is a fully-populated
    /// response with `flagged: false` rather than an error or an empty
    /// message: "this message is fine" is an answer a client asked for.
    fn to_proto(&self, message_id: i64, flag: Option<&Flag>) -> ScanInjectionResponse {
        let Some(flag) = flag else {
            return ScanInjectionResponse {
                message_id,
                flagged: false,
                severity: InjectionSeverity::Unspecified as i32,
                kinds: Vec::new(),
                detections: Vec::new(),
                scanned_at: 0,
                confirmed_at: 0,
                actions_withheld: false,
            };
        };
        ScanInjectionResponse {
            message_id,
            flagged: true,
            severity: match flag.severity {
                rmail_core::ai::injection::Severity::Suspicious => InjectionSeverity::Suspicious,
                rmail_core::ai::injection::Severity::Hostile => InjectionSeverity::Hostile,
            } as i32,
            kinds: flag
                .kinds()
                .into_iter()
                .map(|k| k.as_str().to_owned())
                .collect(),
            detections: flag
                .detections
                .iter()
                .map(|d| InjectionDetection {
                    kind: d.kind.as_str().to_owned(),
                    excerpt: d.excerpt.clone(),
                    // `usize` -> `i64` cannot realistically overflow (an
                    // offset into a message body bounded by
                    // `ai.privacy.max_body_chars`), but saturating rather
                    // than casting keeps a nonsense value out of the wire
                    // instead of a negative one.
                    offset: i64::try_from(d.offset).unwrap_or(i64::MAX),
                })
                .collect(),
            scanned_at: flag.scanned_at,
            confirmed_at: flag.confirmed_at.unwrap_or(0),
            actions_withheld: flag.withholds_actions(&self.injection),
        }
    }
}

/// Reject a non-positive message id before it reaches a query.
///
/// `messages.id` is a SQLite `INTEGER PRIMARY KEY` and is always positive, so
/// a zero or negative id is a client bug; answering `INVALID_ARGUMENT` says
/// so, where letting it through would answer `NOT_FOUND` and read as "that
/// message was deleted".
fn validate_id(message_id: i64) -> Result<i64, Status> {
    if message_id <= 0 {
        return Err(Status::invalid_argument(
            "message_id must be a positive message id",
        ));
    }
    Ok(message_id)
}
