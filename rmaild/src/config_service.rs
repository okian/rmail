//! The `ConfigService` gRPC implementation: the client-side keymap over the
//! wire (task 84).
//!
//! # Why the daemon serves a file it does not own
//!
//! `keys.toml` is client state — the TUI reads it directly and never asks the
//! daemon what `j` means. This service exists for the *other* clients task
//! 84's acceptance names: a command palette or an MCP tool surface (tasks
//! 53/54) has no other way to discover which chords exist, which action ids
//! they resolve to, or what an action is called.
//!
//! # One editor, not two
//!
//! The hazard in serving a file another program also writes is two
//! serializers that disagree. There is only one:
//! [`rmail_core::keymap::file::edit`] is a pure string-to-string transform
//! shared with `mail keys set`, and this module contributes the IO around it
//! and nothing else. A malformed `keys.toml` is refused outright rather than
//! half-rewritten, for the reason `edit` documents — the user needs to hear
//! about their own typo.
//!
//! # Reads are cheap, writes are not
//!
//! `GetKeymap` is `automation` scope: a palette listing what a key does is
//! tooling, not administration. `SetBinding` is `admin`, because it rewrites a
//! file that changes what every keystroke on the machine does. See
//! `auth::methods`.

use std::path::PathBuf;

use rmail_core::keymap::{file, Action, Chord, Keymap, Mode};
use rmail_core::Error as RmailError;
use rmail_proto::v1::config_service_server::ConfigService;
use rmail_proto::v1::{
    ActionInfo, Binding, GetKeymapRequest, GetKeymapResponse, SetBindingRequest, SetBindingResponse,
};
use tonic::{Request, Response, Status};

/// Serves the keymap. Holds only the path, resolved once at startup so every
/// call reads the same file the TUI does.
#[derive(Debug, Clone)]
pub struct ConfigApi {
    keys_path: PathBuf,
}

impl ConfigApi {
    /// Serve the keymap at `keys_path`.
    #[must_use]
    pub const fn new(keys_path: PathBuf) -> Self {
        Self { keys_path }
    }

    /// Read the effective keymap, or map the failure to a `Status`.
    ///
    /// A missing file is not a failure — it means the defaults are in force,
    /// which is the normal state for a user who has never rebound anything.
    /// Returns the domain error, not a `Status`: this crate maps to `Status`
    /// only at the RPC boundary, and a `Result<_, Status>` helper is also what
    /// `clippy::result_large_err` objects to.
    fn read(&self) -> Result<(Keymap, bool), RmailError> {
        let present = self.keys_path.exists();
        let keymap = file::load(&self.keys_path).map_err(|error| {
            RmailError::invalid_argument(format!(
                "{} could not be read: {error}",
                self.keys_path.display()
            ))
        })?;
        Ok((keymap, present))
    }

    /// Flatten a keymap into wire rows.
    ///
    /// Effective bindings, not each layer's own: a client showing the user
    /// what `j` does in the viewer should not have to know the viewer
    /// inherits it from normal mode. `overridden` is computed against a fresh
    /// default map so a client can show which rows the user actually changed.
    fn rows(keymap: &Keymap) -> Vec<Binding> {
        let defaults = Keymap::defaults();
        let mut out = Vec::new();
        for mode in Mode::CONFIGURABLE {
            for action in Action::ALL {
                for chord in keymap.chords_for(*mode, *action) {
                    let overridden = !defaults
                        .chords_for(*mode, *action)
                        .iter()
                        .any(|d| d == &chord);
                    out.push(Binding {
                        mode: mode.id().to_owned(),
                        chord: chord.to_string(),
                        action: action.id().to_owned(),
                        description: action.describe().to_owned(),
                        overridden,
                    });
                }
            }
        }
        out
    }
}

#[tonic::async_trait]
impl ConfigService for ConfigApi {
    #[tracing::instrument(skip(self, _request), err)]
    async fn get_keymap(
        &self,
        _request: Request<GetKeymapRequest>,
    ) -> Result<Response<GetKeymapResponse>, Status> {
        let (keymap, present) = self.read().map_err(Status::from)?;
        Ok(Response::new(GetKeymapResponse {
            bindings: Self::rows(&keymap),
            // Every action, bound or not: an action with no chord is exactly
            // the kind a palette user reaches for by name.
            actions: Action::ALL
                .iter()
                .map(|action| ActionInfo {
                    id: action.id().to_owned(),
                    description: action.describe().to_owned(),
                })
                .collect(),
            keys_path: self.keys_path.display().to_string(),
            keys_file_present: present,
        }))
    }

    #[tracing::instrument(skip(self, request), err)]
    async fn set_binding(
        &self,
        request: Request<SetBindingRequest>,
    ) -> Result<Response<SetBindingResponse>, Status> {
        let req = request.into_inner();

        // `KeymapError` is a config-vocabulary error, not a storage one, so
        // every variant is the caller's fault: `InvalidArgument`, never
        // `Internal`.
        let invalid = |error: rmail_core::keymap::KeymapError| {
            Status::from(RmailError::invalid_argument(error.to_string()))
        };

        let mode = Mode::from_id(&req.mode).ok_or_else(|| {
            Status::from(RmailError::invalid_argument(format!(
                "unknown mode {:?}",
                req.mode
            )))
        })?;
        if !Mode::CONFIGURABLE.contains(&mode) {
            return Err(Status::from(RmailError::invalid_argument(format!(
                "mode {} is not configurable",
                req.mode
            ))));
        }
        let chord = Chord::parse(&req.chord).map_err(invalid)?;
        // Empty unbinds. `edit` removes the line rather than writing a binding
        // to nothing, so the built-in default applies again.
        let action = if req.action.is_empty() {
            None
        } else {
            Some(Action::from_id(&req.action).ok_or_else(|| {
                Status::from(RmailError::invalid_argument(format!(
                    "unknown action {:?}",
                    req.action
                )))
            })?)
        };

        // Validate against a real keymap before touching the file, so a
        // refusal (a reserved chord, say) leaves `keys.toml` exactly as it
        // was. `edit` re-checks, but failing here keeps the file untouched
        // even for the checks only `bind` performs.
        if let Some(action) = action {
            let mut probe = Keymap::defaults();
            probe.bind(mode, chord.clone(), action).map_err(invalid)?;
        }

        let path = self.keys_path.clone();
        let existing = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => {
                return Err(Status::from(RmailError::internal(format!(
                    "reading {}: {error}",
                    path.display()
                ))))
            }
        };
        let updated = file::edit(&existing, mode, &chord, action).map_err(invalid)?;

        // Blocking IO off the runtime: a slow or full disk must not stall
        // every other RPC in flight.
        let write_path = path.clone();
        tokio::task::spawn_blocking(move || file::write_atomic(&write_path, &updated))
            .await
            .map_err(|error| {
                Status::from(RmailError::internal(format!("keymap write task: {error}")))
            })?
            .map_err(|error| {
                Status::from(RmailError::internal(format!(
                    "writing {}: {error}",
                    path.display()
                )))
            })?;

        let (keymap, _) = self.read().map_err(Status::from)?;
        Ok(Response::new(SetBindingResponse {
            bindings: Self::rows(&keymap),
            keys_path: self.keys_path.display().to_string(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// A `ConfigApi` over a scratch `keys.toml` that does not exist yet.
    ///
    /// Driven directly rather than through the tonic server: the service holds
    /// its path, so a test needs no socket, and going via the daemon would
    /// mean setting `RMAIL_KEYS` — process-global state that two tests running
    /// at once would fight over.
    fn fixture() -> (ConfigApi, PathBuf) {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("rmail-keys-{}-{n}/keys.toml", std::process::id()));
        let _ = std::fs::remove_dir_all(path.parent().unwrap_or(&path));
        (ConfigApi::new(path.clone()), path)
    }

    #[tokio::test]
    async fn get_keymap_serves_the_defaults_and_the_whole_action_registry() {
        let (api, path) = fixture();
        let out = api
            .get_keymap(Request::new(GetKeymapRequest {}))
            .await
            .unwrap()
            .into_inner();

        assert!(!out.keys_file_present, "no file was written");
        assert_eq!(out.keys_path, path.display().to_string());
        assert!(
            !out.bindings.is_empty(),
            "the built-in defaults are the keymap when no file exists"
        );
        assert!(
            out.bindings.iter().all(|b| !b.overridden),
            "nothing is overridden when there is no keys.toml"
        );
        // The registry, not just what happens to be bound — a palette resolves
        // to actions no chord reaches.
        assert_eq!(out.actions.len(), Action::ALL.len());
        assert!(out
            .actions
            .iter()
            .all(|a| !a.id.is_empty() && !a.description.is_empty()));
    }

    #[tokio::test]
    async fn set_binding_writes_the_file_and_reports_the_new_binding() {
        let (api, path) = fixture();
        let out = api
            .set_binding(Request::new(SetBindingRequest {
                mode: "normal".to_owned(),
                chord: "<c-d>".to_owned(),
                action: "cursor.down".to_owned(),
            }))
            .await
            .unwrap()
            .into_inner();

        let bound = out
            .bindings
            .iter()
            .find(|b| b.chord == "<c-d>" && b.mode == "normal")
            .expect("the new binding is in the response");
        assert_eq!(bound.action, "cursor.down");
        assert!(bound.overridden, "it differs from the defaults");
        assert!(path.exists(), "keys.toml was created");
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("cursor.down"));
    }

    #[tokio::test]
    async fn an_empty_action_unbinds_and_restores_the_default() {
        let (api, path) = fixture();
        // Rebind `j`, then unbind it.
        api.set_binding(Request::new(SetBindingRequest {
            mode: "normal".to_owned(),
            chord: "j".to_owned(),
            action: "message.archive".to_owned(),
        }))
        .await
        .unwrap();
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("message.archive"));

        let out = api
            .set_binding(Request::new(SetBindingRequest {
                mode: "normal".to_owned(),
                chord: "j".to_owned(),
                action: String::new(),
            }))
            .await
            .unwrap()
            .into_inner();

        // The line is gone, so the built-in default is back — not a binding
        // to nothing.
        assert!(!std::fs::read_to_string(&path)
            .unwrap()
            .contains("message.archive"));
        let j = out
            .bindings
            .iter()
            .find(|b| b.chord == "j" && b.mode == "normal")
            .expect("j is bound again by the defaults");
        assert_eq!(j.action, "cursor.down");
        assert!(!j.overridden);
    }

    /// The one refusal that matters: a mode nobody can leave is the worst
    /// failure a modal UI has, so a chord starting with `Esc` or `Ctrl-C` is
    /// refused — and the file must not be touched on the way to refusing.
    #[tokio::test]
    async fn a_reserved_chord_is_refused_without_writing_anything() {
        let (api, path) = fixture();
        for chord in ["<esc>", "<c-c>", "<esc>j"] {
            let status = api
                .set_binding(Request::new(SetBindingRequest {
                    mode: "normal".to_owned(),
                    chord: chord.to_owned(),
                    action: "cursor.down".to_owned(),
                }))
                .await
                .expect_err("a reserved chord must be refused");
            assert_eq!(status.code(), tonic::Code::InvalidArgument, "{chord}");
        }
        assert!(!path.exists(), "a refused edit writes no file at all");
    }

    #[tokio::test]
    async fn an_unknown_mode_or_action_is_invalid_argument_not_a_write() {
        let (api, path) = fixture();
        let bad_mode = api
            .set_binding(Request::new(SetBindingRequest {
                mode: "wrok".to_owned(),
                chord: "j".to_owned(),
                action: "cursor.down".to_owned(),
            }))
            .await
            .expect_err("unknown mode");
        assert_eq!(bad_mode.code(), tonic::Code::InvalidArgument);

        let bad_action = api
            .set_binding(Request::new(SetBindingRequest {
                mode: "normal".to_owned(),
                chord: "j".to_owned(),
                action: "no.such.action".to_owned(),
            }))
            .await
            .expect_err("unknown action");
        assert_eq!(bad_action.code(), tonic::Code::InvalidArgument);
        assert!(!path.exists());
    }

    /// A `keys.toml` the user has already broken is refused rather than
    /// half-rewritten — they need to hear about their own typo.
    #[tokio::test]
    async fn a_malformed_keys_file_is_refused_rather_than_rewritten() {
        let (api, path) = fixture();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let broken = "[normal]\n\"j\" = \n";
        std::fs::write(&path, broken).unwrap();

        let status = api
            .set_binding(Request::new(SetBindingRequest {
                mode: "normal".to_owned(),
                chord: "k".to_owned(),
                action: "cursor.up".to_owned(),
            }))
            .await
            .expect_err("a broken file is refused");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            broken,
            "the broken file is left exactly as the user wrote it"
        );
    }
}
