//! The OAuth2 broker: loopback-redirect authorization with PKCE for Google and
//! Microsoft, refresh-token custody, and the XOAUTH2 SASL string IMAP and SMTP
//! authenticate with (task 79).
//!
//! # Why loopback + PKCE and nothing else
//!
//! rmail is a native application. It cannot keep a client secret, and the two
//! providers it targets both say so: Google's "Desktop app" and Microsoft's
//! "Mobile and desktop applications" client types are public clients, and both
//! document the loopback-interface redirect (RFC 8252 §7.3) as the flow for
//! them. PKCE (RFC 7636) is what makes that safe — the authorization code is
//! delivered over a plain HTTP socket on `127.0.0.1`, where any other local
//! process that can guess the port could try to race for it, and without a
//! `code_verifier` a stolen code is a complete account takeover. The verifier
//! never leaves this process, so a code intercepted on loopback is worthless.
//!
//! An `http://127.0.0.1:<ephemeral>` redirect is used rather than a fixed port:
//! a fixed port is a port another application may already hold, and both
//! providers permit any port on the loopback host for this exact reason.
//! `localhost` is deliberately *not* used — it resolves through the host's
//! resolver and can be pointed elsewhere.
//!
//! # Everything here is a secret, including the things that do not look like one
//!
//! The refresh token is the durable credential: it is a bearer credential for
//! the user's entire mailbox with no expiry. The access token is the same thing
//! with an hour on it. The authorization code is one exchange away from both.
//! The `code_verifier` is what stops a stolen code from being spent, and
//! `state` is what stops a foreign code from being planted. All five are
//! [`Secret`]s, which is why none of them can reach a log, a `Debug` line, an
//! error message, or a gRPC response — a `format!("{:?}")` of anything in this
//! module prints `Secret(***)`.
//!
//! # Refreshing is serialized per account, on purpose
//!
//! A sync pass opens connections for several folders at once, and they all
//! notice the same expired access token in the same millisecond. Left alone
//! that is N concurrent refreshes: N times the rate-limit budget, and — on
//! Microsoft, which *rotates* the refresh token on every use — N-1 of them
//! invalidated by whichever one landed last, leaving the store holding a token
//! the provider has already retired. So [`OAuthBroker::access_token`] takes a
//! per-account lock across the whole load/refresh/store sequence and
//! re-examines the cache under it: the first caller refreshes and the rest read
//! what it wrote.
//!
//! That invariant is per *keychain item*, not per broker object, so a process
//! must have exactly one broker over a given store. `rmaild` builds one and
//! shares it; a second one over the same keychain would race the first.

mod pkce;
mod provider;
mod redirect;
mod store;
#[cfg(test)]
mod tests;
mod url;

pub use pkce::Pkce;
pub use provider::Provider;
pub use redirect::{LoopbackRedirect, AUTHORIZATION_TIMEOUT};
pub use store::{KeychainTokenStore, MemoryTokenStore, StoreKey, StoredTokens, TokenStore};

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;

use crate::credential::Secret;
use crate::error::Error;

/// How long a request to a token endpoint may take.
///
/// Bounded because this sits in front of every IMAP connection an OAuth
/// account makes: a token endpoint that hangs must become an error a sync can
/// report, not a sync that never starts.
const TOKEN_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// How far before its stated expiry an access token is treated as spent.
///
/// A token that expires in ten seconds is useless: the IMAP handshake it is
/// about to be used for takes longer than that, and the failure arrives as an
/// authentication error mid-sync rather than as a refresh. Two minutes also
/// absorbs the ordinary disagreement between this machine's clock and the
/// provider's, which is the other way a token that "has not expired yet"
/// arrives already expired.
const REFRESH_SKEW: Duration = Duration::from_secs(120);

/// The longest remaining lifetime a stored access token is believed to have.
///
/// `expires_in` is relative, so the absolute expiry written to the store is
/// only as good as the clock that computed it. A machine whose clock was hours
/// fast when the token was stored — or which has since been stepped backwards
/// by NTP — leaves an `expires_at` far in the future for a token the provider
/// retired long ago, and nothing about the token itself says so. Both
/// providers issue access tokens measured in minutes to an hour, so anything
/// claiming more than this is evidence of a bad clock rather than of a
/// long-lived token, and is refreshed instead of trusted.
const MAX_TOKEN_LIFETIME: Duration = Duration::from_secs(24 * 60 * 60);

/// How long one credential-store operation may take.
///
/// The store is the macOS Keychain, which can raise a modal prompt; see
/// [`OAuthBroker::in_store`] for why an unbounded wait there is not merely
/// slow but permanently wedging.
const STORE_TIMEOUT: Duration = Duration::from_secs(30);

/// Assumed access-token lifetime when the provider omits `expires_in`.
///
/// The parameter is `RECOMMENDED`, not required, by RFC 6749 §5.1. An absent
/// one must not mean "never expires" — that is a token that is refreshed only
/// after it has already failed — so it means the shortest lifetime either
/// provider actually issues.
const DEFAULT_EXPIRES_IN: i64 = 3600;

/// The result of completing or refreshing an authorization, as reported to a
/// caller. Deliberately carries no token material.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenStatus {
    /// The provider the account authenticates against.
    pub provider: Provider,
    /// When the current access token expires (unix seconds).
    pub expires_at: i64,
    /// The scopes the provider actually granted.
    pub scopes: Vec<String>,
    /// Whether this call performed a network refresh.
    pub refreshed: bool,
}

/// A prepared authorization request: the loopback listener is already bound
/// (so the redirect URI is known and cannot be stolen by another process
/// binding the port first) and the PKCE pair is already generated.
#[derive(Debug)]
pub struct PendingAuthorization {
    provider: Provider,
    client_id: String,
    client_secret: Option<Secret>,
    scopes: Vec<String>,
    pkce: Pkce,
    redirect: LoopbackRedirect,
}

impl PendingAuthorization {
    /// The URL the user must open to consent.
    ///
    /// Safe to display and to log — it carries the public half of the PKCE
    /// pair (the challenge, a hash) and the `state`, which is why
    /// [`PendingAuthorization::authorization_url`] is the only accessor and
    /// nothing here logs it on the caller's behalf: `state` is a CSRF token
    /// and belongs in front of the user, not in a log file shared with
    /// whoever reads the daemon's output.
    #[must_use]
    pub fn authorization_url(&self) -> String {
        self.provider.authorization_url(
            &self.client_id,
            self.redirect.redirect_uri(),
            &self.scopes,
            // The one place `state` is exposed. It has to travel in the URL —
            // that is what it is for — but the exposure is written out here so
            // that nothing else can reach for it casually.
            self.redirect.state().expose(),
            self.pkce.challenge(),
        )
    }

    /// The loopback URI the provider will redirect to.
    #[must_use]
    pub fn redirect_uri(&self) -> &str {
        self.redirect.redirect_uri()
    }

    /// Which provider this authorization is against.
    #[must_use]
    pub fn provider(&self) -> Provider {
        self.provider
    }
}

/// The broker: token custody, refresh, and the authorization flow.
///
/// Holds no tokens of its own beyond an in-memory cache of what the store
/// already has; the durable copy lives in the [`TokenStore`] (the macOS
/// Keychain in production).
pub struct OAuthBroker {
    store: Arc<dyn TokenStore>,
    http: reqwest::Client,
    /// Overrides the provider token endpoints. Only tests set this; production
    /// always talks to the provider [`Provider::token_endpoint`] names.
    token_endpoint_override: Option<String>,
    /// One lock per stored credential, guarding the load/refresh/store
    /// sequence. See the module docs: this is what keeps concurrent callers
    /// from burning N refreshes and racing each other's writes.
    locks: std::sync::Mutex<HashMap<StoreKey, Arc<Mutex<()>>>>,
    /// The most recent token set seen for a key, so the common case does not
    /// re-read the Keychain (which prompts, and is slow) on every connection.
    cache: std::sync::Mutex<HashMap<StoreKey, StoredTokens>>,
    /// Grants the provider has already told us are dead, as a digest of the
    /// refresh token that was rejected.
    ///
    /// `invalid_grant` is final: no retry fixes it, only a browser. Without
    /// this, every IMAP connection and every SMTP send for a revoked account
    /// posts to the token endpoint to be told the same thing again — a request
    /// storm against a provider that has already said no, which is exactly the
    /// "retry loop" the revoked path is supposed to avoid. Keyed by digest
    /// rather than by account so that re-consent (which changes the refresh
    /// token) clears it automatically even if nothing thought to.
    revoked: std::sync::Mutex<HashMap<StoreKey, String>>,
}

impl std::fmt::Debug for OAuthBroker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Hand-written rather than derived: the cache holds `StoredTokens`,
        // whose own `Debug` redacts, but printing the *keys* would still name
        // every account this daemon holds credentials for. There is nothing
        // here worth printing.
        f.debug_struct("OAuthBroker").finish_non_exhaustive()
    }
}

impl OAuthBroker {
    /// A broker over `store`.
    ///
    /// # Errors
    ///
    /// [`Error::FailedPrecondition`] if the HTTP client cannot be built.
    pub fn new(store: Arc<dyn TokenStore>) -> Result<Self, Error> {
        // As in the IMAP client and the Voyage embedder: the crypto provider is
        // installed explicitly rather than inferred from crate features, since
        // inference panics on the first handshake once a second provider is
        // linked in.
        crate::transport::install_crypto_provider();
        let http = reqwest::Client::builder()
            .timeout(TOKEN_REQUEST_TIMEOUT)
            // A 307/308 from a token endpoint would re-POST the body — which
            // is the refresh token — to wherever the redirect points. There is
            // no legitimate reason for a token endpoint to redirect, so the
            // safe policy is to refuse rather than to follow.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| {
                Error::failed_precondition(format!("could not build an HTTP client: {e}"))
            })?;
        Ok(Self {
            store,
            http,
            token_endpoint_override: None,
            locks: std::sync::Mutex::new(HashMap::new()),
            cache: std::sync::Mutex::new(HashMap::new()),
            revoked: std::sync::Mutex::new(HashMap::new()),
        })
    }

    /// Point every token request at `endpoint` instead of the provider's.
    ///
    /// Exists so tests can drive a local server, and is why it refuses
    /// anything but `https://` or a loopback host: this is public API on a
    /// library crate, and an override that accepted an arbitrary `http://`
    /// URL would be a supported way to redirect every refresh token this
    /// daemon holds to a third party in cleartext.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgument`] for a non-`https` endpoint that is not on
    /// the loopback interface.
    pub fn with_token_endpoint(mut self, endpoint: impl Into<String>) -> Result<Self, Error> {
        let endpoint = endpoint.into();
        let loopback = endpoint.starts_with("http://127.0.0.1:")
            || endpoint.starts_with("http://[::1]:")
            || endpoint.starts_with("http://localhost:");
        if !endpoint.starts_with("https://") && !loopback {
            return Err(Error::invalid_argument(
                "an OAuth token endpoint must be https, or a loopback address for tests",
            ));
        }
        self.token_endpoint_override = Some(endpoint);
        Ok(self)
    }

    /// Begin an authorization: bind the loopback listener, generate the PKCE
    /// pair and the `state`, and return everything needed to send the user to
    /// the provider.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`] if no loopback port can be bound;
    /// [`Error::InvalidArgument`] for an empty `client_id`.
    #[tracing::instrument(skip(self, client_secret), fields(provider = provider.as_str()))]
    pub async fn begin(
        &self,
        provider: Provider,
        client_id: &str,
        client_secret: Option<Secret>,
        scopes: Option<Vec<String>>,
    ) -> Result<PendingAuthorization, Error> {
        let client_id = client_id.trim();
        if client_id.is_empty() {
            return Err(Error::invalid_argument(
                "an OAuth client id is required; register a desktop/native \
                 application with the provider and pass its client id",
            ));
        }
        let scopes = match scopes {
            Some(scopes) if !scopes.is_empty() => scopes,
            _ => provider.default_scopes(),
        };
        let redirect = LoopbackRedirect::bind().await?;
        tracing::debug!(
            redirect_uri = redirect.redirect_uri(),
            "bound the OAuth loopback redirect"
        );
        Ok(PendingAuthorization {
            provider,
            client_id: client_id.to_owned(),
            client_secret,
            scopes,
            pkce: Pkce::generate(),
            redirect,
        })
    }

    /// Wait for the provider's redirect, exchange the code, and persist the
    /// resulting tokens under `key`.
    ///
    /// # Errors
    ///
    /// [`Error::Unauthenticated`] if the user declined or the provider
    /// rejected the exchange; [`Error::DeadlineExceeded`] if no redirect
    /// arrives within [`AUTHORIZATION_TIMEOUT`]; [`Error::Cancelled`] if
    /// `cancel` fires first.
    #[tracing::instrument(
        skip(self, key, pending, cancel),
        fields(provider = pending.provider.as_str())
    )]
    pub async fn complete(
        &self,
        key: &StoreKey,
        pending: PendingAuthorization,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<TokenStatus, Error> {
        let code = pending.redirect.wait_for_code(cancel).await?;

        // Taken *after* the wait, not before: the wait is a human in a browser
        // and can last five minutes, and holding the account's lock across it
        // would stall every sync for that account for the duration.
        //
        // Taken at all because the exchange and the write that follows are the
        // same critical section `current` protects. Without it, a refresh that
        // loaded the old tokens before this ran can persist them *after* it,
        // overwriting a grant the user has just re-consented to with the very
        // refresh token they re-consented to replace.
        let lock = self.lock_for(key);
        let _guard = lock.lock().await;

        let response = self
            .post_token(
                pending.provider,
                &[
                    ("grant_type", "authorization_code"),
                    ("code", code.expose()),
                    ("code_verifier", pending.pkce.verifier().expose()),
                    ("client_id", &pending.client_id),
                    ("redirect_uri", pending.redirect.redirect_uri()),
                ],
                pending.client_secret.as_ref(),
            )
            .await?;

        let scopes = response.scopes();
        let refresh_token = response.refresh_token.ok_or_else(|| {
            // Google returns a refresh token only for a first consent, or when
            // `access_type=offline&prompt=consent` is asked for (which
            // `Provider::authorization_url` always does). Without one there is
            // nothing to store and every future sync would need the browser
            // again, so this is a failed authorization rather than a partial
            // success worth persisting.
            Error::failed_precondition(
                "the provider returned no refresh token; revoke rmail's access in \
                 your account's security settings and authorize again",
            )
        })?;

        let tokens = StoredTokens {
            provider: pending.provider,
            client_id: pending.client_id,
            client_secret: pending.client_secret,
            refresh_token,
            access_token: Some(response.access_token),
            expires_at: expiry_from(response.expires_in),
            scopes,
        };
        let status = tokens.status(true);
        // Failure is returned here, unlike in `current`: a user is watching,
        // an authorization they can simply run again is a far better outcome
        // than an account repointed at a Keychain item that was never written,
        // and — unlike a refresh — nothing has been invalidated upstream by
        // giving up.
        self.persist(key, tokens).await?;
        // Any revoked verdict recorded against the grant this replaces is now
        // stale by construction: the user has just re-consented.
        self.clear_revoked(key);
        tracing::info!(
            provider = status.provider.as_str(),
            expires_at = status.expires_at,
            "stored OAuth tokens"
        );
        Ok(status)
    }

    /// A valid access token for `key`, refreshing first if the stored one is
    /// spent.
    ///
    /// # Errors
    ///
    /// [`Error::FailedPrecondition`] if no tokens are stored (the account has
    /// never been authorized); [`Error::Unauthenticated`] if the refresh token
    /// has been revoked or the provider rejected it.
    pub async fn access_token(&self, key: &StoreKey) -> Result<Secret, Error> {
        let (tokens, _) = self.current(key, false).await?;
        tokens.access_token.ok_or_else(|| {
            // Unreachable via `current`, which only returns a set whose access
            // token it has just validated or replaced. Handled rather than
            // asserted because the alternative is an `expect` in a credential
            // path.
            Error::internal("the OAuth broker produced no access token")
        })
    }

    /// Refresh `key`'s access token, unconditionally when `force` is set.
    ///
    /// # Errors
    ///
    /// As [`OAuthBroker::access_token`].
    #[tracing::instrument(skip(self, key), fields(force))]
    pub async fn refresh(&self, key: &StoreKey, force: bool) -> Result<TokenStatus, Error> {
        let (tokens, refreshed) = self.current(key, force).await?;
        Ok(tokens.status(refreshed))
    }

    /// The stored status for `key` without touching the network.
    ///
    /// # Errors
    ///
    /// [`Error::FailedPrecondition`] if no tokens are stored.
    pub async fn status(&self, key: &StoreKey) -> Result<TokenStatus, Error> {
        let tokens = self.load(key).await?;
        Ok(tokens.status(false))
    }

    /// Forget `key`'s tokens, in the store and in the cache.
    ///
    /// # Errors
    ///
    /// A store error.
    pub async fn forget(&self, key: &StoreKey) -> Result<(), Error> {
        // Under the same lock the refresh path uses: otherwise an in-flight
        // refresh can `persist` after this delete and resurrect the grant the
        // caller just asked to be forgotten.
        let lock = self.lock_for(key);
        let _guard = lock.lock().await;
        let store = Arc::clone(&self.store);
        let owned = key.clone();
        self.in_store(move || store.delete(&owned)).await?;
        self.cache_remove(key);
        self.clear_revoked(key);
        Ok(())
    }

    /// The load/validate/refresh sequence, serialized per key.
    ///
    /// Returns the token set and whether this call went to the network. The
    /// lock is taken *before* the cache is consulted and held across the whole
    /// sequence: a check outside the lock followed by a refresh inside it is
    /// exactly the race this exists to prevent, since every concurrent caller
    /// would pass the check.
    async fn current(&self, key: &StoreKey, force: bool) -> Result<(StoredTokens, bool), Error> {
        let lock = self.lock_for(key);
        let _guard = lock.lock().await;

        let tokens = self.load(key).await?;
        if !force && !is_spent(&tokens, now()) {
            return Ok((tokens, false));
        }
        // `force` is the caller saying "I know, ask anyway" — the escape hatch
        // behind `mail account refresh --force` for someone who has just fixed
        // things at the provider's end.
        if !force {
            if let Some(verdict) = self.revoked_verdict(key, &tokens.refresh_token) {
                return Err(verdict);
            }
        }

        let refreshed = match self.exchange_refresh(&tokens).await {
            Ok(refreshed) => refreshed,
            Err(error) => {
                if error.reason() == crate::ErrorReason::Unauthenticated {
                    self.mark_revoked(key, &tokens.refresh_token);
                }
                return Err(error);
            }
        };
        self.clear_revoked(key);
        // A refresh may return no `scope`, meaning "unchanged" (RFC 6749
        // §5.1); an empty list would otherwise erase what was granted.
        let scopes = {
            let returned = refreshed.scopes();
            if returned.is_empty() {
                tokens.scopes
            } else {
                returned
            }
        };
        let updated = StoredTokens {
            provider: tokens.provider,
            client_id: tokens.client_id,
            client_secret: tokens.client_secret,
            // Microsoft rotates the refresh token on every use and Google does
            // not return one at all; keep the old one unless a new one arrived,
            // or the next refresh presents a token the provider has retired.
            refresh_token: refreshed.refresh_token.unwrap_or(tokens.refresh_token),
            expires_at: expiry_from(refreshed.expires_in),
            access_token: Some(refreshed.access_token),
            scopes,
        };
        let out = updated.clone();
        // Cached before the store is written, and the write failure is logged
        // rather than returned. The refresh has *already happened* at the
        // provider by this point: on Microsoft that retired the refresh token
        // the store still holds, so returning an error here would fail a sync
        // that has a perfectly good access token in hand and would leave the
        // process retrying with a token the provider will never accept again.
        //
        // Caching first turns a locked Keychain into "this daemon keeps
        // working until it restarts, loudly", which is the best outcome
        // available — the durable copy is genuinely stale, and only re-consent
        // can fix that, so the log is at `error` and says so.
        //
        // `complete` deliberately does *not* do this: there a store failure
        // must fail the call, because a user is watching and an account
        // repointed at a Keychain item that was never written is worse than an
        // authorization they can simply run again.
        self.cache_put(key, &updated);
        if let Err(error) = self.persist(key, updated).await {
            tracing::error!(
                %error,
                provider = out.provider.as_str(),
                "refreshed an OAuth token but could not write it to the credential store; \
                 this daemon will keep working until it restarts, after which the account \
                 must be authorized again"
            );
        } else {
            tracing::debug!(
                provider = out.provider.as_str(),
                expires_at = out.expires_at,
                "refreshed an OAuth access token"
            );
        }
        Ok((out, true))
    }

    /// The per-key lock, created on first use.
    fn lock_for(&self, key: &StoreKey) -> Arc<Mutex<()>> {
        let mut locks = self
            .locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::clone(locks.entry(key.clone()).or_default())
    }

    /// The cached token set for `key`, reading the store on a miss.
    async fn load(&self, key: &StoreKey) -> Result<StoredTokens, Error> {
        if let Some(hit) = self.cache_get(key) {
            return Ok(hit);
        }
        let store = Arc::clone(&self.store);
        let owned = key.clone();
        let loaded = self.in_store(move || store.load(&owned)).await?;
        let tokens = loaded.ok_or_else(|| {
            Error::failed_precondition(format!(
                "no OAuth tokens are stored for {}; run `mail account login --oauth <provider>`",
                key.describe()
            ))
        })?;
        self.cache_put(key, &tokens);
        Ok(tokens)
    }

    async fn persist(&self, key: &StoreKey, tokens: StoredTokens) -> Result<(), Error> {
        let store = Arc::clone(&self.store);
        let owned = key.clone();
        let to_write = tokens.clone();
        self.in_store(move || store.save(&owned, &to_write)).await?;
        self.cache_put(key, &tokens);
        Ok(())
    }

    /// Run one store operation off the runtime, under a deadline.
    ///
    /// The Keychain blocks — on macOS it can put a *modal prompt* in front of
    /// the user, which never returns if nobody is at the machine. Every store
    /// call happens while this account's lock is held, so without a deadline
    /// one unanswered prompt pins a blocking-pool thread and wedges every
    /// connection for that account forever. `access_token` is deliberately
    /// outside `IMAP_DEADLINE` (see `imap::conn::connect_account`), so this is
    /// the only bound on that path.
    async fn in_store<T, F>(&self, operation: F) -> Result<T, Error>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T, Error> + Send + 'static,
    {
        let task = tokio::task::spawn_blocking(operation);
        match tokio::time::timeout(STORE_TIMEOUT, task).await {
            Ok(joined) => {
                joined.map_err(|e| Error::internal(format!("token store task failed: {e}")))?
            }
            // The task itself is left running: aborting a `spawn_blocking` is
            // not possible, and the thread will finish (or stay blocked on the
            // prompt) either way. What matters is that the caller and the
            // account's lock are released.
            Err(_) => Err(Error::deadline_exceeded(
                "the OAuth credential store did not respond; if a keychain \
                 prompt is waiting, answer it and try again",
            )),
        }
    }

    /// The recorded `invalid_grant` verdict for `key`, if it was recorded
    /// against the refresh token still being presented.
    fn revoked_verdict(&self, key: &StoreKey, refresh_token: &Secret) -> Option<Error> {
        let digest = digest_of(refresh_token);
        let matches = self
            .revoked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(key)
            .is_some_and(|recorded| *recorded == digest);
        matches.then(|| {
            tracing::debug!("short-circuiting a refresh of a grant the provider already revoked");
            revoked_error(
                self.cache_get(key)
                    .map_or(Provider::Google, |tokens| tokens.provider),
            )
        })
    }

    fn mark_revoked(&self, key: &StoreKey, refresh_token: &Secret) {
        self.revoked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key.clone(), digest_of(refresh_token));
    }

    fn clear_revoked(&self, key: &StoreKey) {
        self.revoked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(key);
    }

    fn cache_get(&self, key: &StoreKey) -> Option<StoredTokens> {
        self.cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(key)
            .cloned()
    }

    fn cache_put(&self, key: &StoreKey, tokens: &StoredTokens) {
        self.cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key.clone(), tokens.clone());
    }

    fn cache_remove(&self, key: &StoreKey) {
        self.cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(key);
    }

    async fn exchange_refresh(&self, tokens: &StoredTokens) -> Result<TokenResponse, Error> {
        self.post_token(
            tokens.provider,
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", tokens.refresh_token.expose()),
                ("client_id", &tokens.client_id),
            ],
            tokens.client_secret.as_ref(),
        )
        .await
    }

    /// One form POST to a token endpoint, with the whole error taxonomy.
    ///
    /// Neither the form nor the response body is ever logged: the form carries
    /// the authorization code or the refresh token, and the body carries the
    /// new access token.
    async fn post_token(
        &self,
        provider: Provider,
        form: &[(&str, &str)],
        client_secret: Option<&Secret>,
    ) -> Result<TokenResponse, Error> {
        let endpoint = self
            .token_endpoint_override
            .as_deref()
            .unwrap_or_else(|| provider.token_endpoint());
        let mut form: Vec<(&str, &str)> = form.to_vec();
        // Public clients have none. Google nonetheless issues one to "Desktop
        // app" clients and rejects an exchange without it, which is why this is
        // optional rather than absent: the secret is not a secret in a native
        // application, but the provider still requires the field.
        if let Some(secret) = client_secret {
            form.push(("client_secret", secret.expose()));
        }

        let response = self
            .http
            .post(endpoint)
            .form(&form)
            .send()
            .await
            .map_err(|e| {
                // `e` carries the URL and the transport failure, never the
                // form — `reqwest` does not put the body in its error.
                Error::unavailable(format!("the OAuth token request failed: {e}"))
            })?;

        let status = response.status();
        let body = response.text().await.map_err(|_| {
            Error::unavailable("the OAuth token response could not be read".to_owned())
        })?;

        if !status.is_success() {
            return Err(classify_token_error(provider, status.as_u16(), &body));
        }

        serde_json::from_str::<TokenResponse>(&body).map_err(|e| {
            // The body is *never* echoed, not even at debug: a 200 that fails
            // to parse still very often contains a valid access token next to
            // the field that broke the parse.
            tracing::warn!(
                provider = provider.as_str(),
                error = %e,
                "the OAuth token response did not parse"
            );
            Error::unavailable(
                "the OAuth token response was malformed (no usable access token)".to_owned(),
            )
        })
    }
}

/// Whether a stored token set needs refreshing before use.
///
/// Three ways to be spent, all of which must refresh rather than be trusted:
/// no access token at all (a fresh store, or a daemon restart), an expiry
/// within [`REFRESH_SKEW`], and an expiry so far out that the clock which
/// computed it cannot be believed (see [`MAX_TOKEN_LIFETIME`]).
fn is_spent(tokens: &StoredTokens, now: i64) -> bool {
    if tokens.access_token.is_none() {
        return true;
    }
    let skew = i64::try_from(REFRESH_SKEW.as_secs()).unwrap_or(i64::MAX);
    let max = i64::try_from(MAX_TOKEN_LIFETIME.as_secs()).unwrap_or(i64::MAX);
    tokens.expires_at <= now.saturating_add(skew) || tokens.expires_at > now.saturating_add(max)
}

/// Absolute expiry from a relative `expires_in`.
fn expiry_from(expires_in: Option<i64>) -> i64 {
    // A zero or negative `expires_in` is a provider saying the token is
    // already dead; `saturating_add` of a non-positive value leaves an expiry
    // in the past, which `is_spent` reads as "refresh", which is correct.
    now().saturating_add(expires_in.unwrap_or(DEFAULT_EXPIRES_IN))
}

/// Now, in unix seconds.
///
/// Re-exported as [`unix_now`] so callers building a [`StoredTokens`] compute
/// `expires_at` on the same clock the broker reads it back on.
fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Translate a non-2xx token-endpoint response into the domain error model.
///
/// The distinction that matters is "the user must consent again" versus "try
/// later": collapsing them turns a revoked grant into an infinite retry loop
/// that never tells anyone to open a browser, and turns a rate limit into a
/// spurious demand for re-consent.
fn classify_token_error(provider: Provider, status: u16, body: &str) -> Error {
    let code = oauth_error_code(body);
    match code.as_deref() {
        // RFC 6749 §5.2. Both providers return `invalid_grant` when the user
        // revoked access, changed their password, or the grant simply aged
        // out. There is no retry that fixes it.
        Some("invalid_grant" | "expired_token") => revoked_error(provider),
        Some("invalid_client" | "unauthorized_client") => Error::unauthenticated(format!(
            "{} rejected the OAuth client id/secret for this application",
            provider.display_name()
        )),
        Some("access_denied") => Error::permission_denied(format!(
            "{} denied the authorization request",
            provider.display_name()
        )),
        _ => match status {
            401 | 403 => Error::unauthenticated(format!(
                "{} rejected the OAuth credentials ({status})",
                provider.display_name()
            )),
            429 | 500..=599 => Error::unavailable(format!(
                "{} could not issue a token right now ({status})",
                provider.display_name()
            )),
            // `FailedPrecondition`, not `InvalidArgument`: the caller's request
            // was fine — they named an account id — and it is the stored OAuth
            // client configuration the provider is objecting to. Telling a
            // `RefreshToken` caller their argument was invalid sends them
            // looking in the wrong place entirely.
            _ => Error::failed_precondition(format!(
                "{} rejected the token request ({status}{}); check the OAuth client \
                 configuration for this account",
                provider.display_name(),
                code.map(|c| format!("; {c}")).unwrap_or_default()
            )),
        },
    }
}

/// The one wording for "consent is gone, open a browser".
///
/// Shared between the provider's own `invalid_grant` and the short-circuit
/// that remembers it, so a caller cannot tell the cached verdict from the
/// fresh one — and so the two can never drift into saying different things.
fn revoked_error(provider: Provider) -> Error {
    Error::unauthenticated(format!(
        "{} has revoked or expired rmail's authorization; \
         re-authorize with `mail account login --oauth {}`",
        provider.display_name(),
        provider.as_str()
    ))
}

/// A stable, non-reversible fingerprint of a secret, for keying a decision on
/// "is this still the same credential" without keeping a second copy of it.
fn digest_of(secret: &Secret) -> String {
    use sha2::{Digest as _, Sha256};
    format!("{:x}", Sha256::digest(secret.expose().as_bytes()))
}

/// The `error` field of an RFC 6749 §5.2 error body, if it has one.
///
/// Only the code is read. `error_description` is provider-authored free text
/// that has been observed to quote the request back, so repeating it into a
/// `tonic::Status` message risks echoing the refresh token to whoever reads
/// the error.
fn oauth_error_code(body: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(body).ok()?;
    let code = parsed.get("error")?;
    // Microsoft answers with `{"error": "invalid_grant", ...}`; some servers
    // nest it as `{"error": {"code": "..."}}`.
    code.as_str()
        .map(str::to_owned)
        .or_else(|| code.get("code")?.as_str().map(str::to_owned))
}

/// A token-endpoint success body (RFC 6749 §5.1).
#[derive(serde::Deserialize)]
struct TokenResponse {
    access_token: Secret,
    #[serde(default)]
    refresh_token: Option<Secret>,
    /// Seconds until the access token expires. `RECOMMENDED`, not required.
    ///
    /// Deserialized permissively because providers have shipped it as a JSON
    /// string; a value that will not parse is treated as absent rather than
    /// failing the whole response, since the fallback is a shorter assumed
    /// lifetime and therefore safe.
    #[serde(default, deserialize_with = "lenient_seconds")]
    expires_in: Option<i64>,
    /// Space-delimited granted scopes, per RFC 6749.
    #[serde(default)]
    scope: Option<String>,
}

impl TokenResponse {
    fn scopes(&self) -> Vec<String> {
        self.scope
            .as_deref()
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_owned)
            .collect()
    }
}

fn lenient_seconds<'de, D>(deserializer: D) -> Result<Option<i64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize as _;
    Ok(match serde_json::Value::deserialize(deserializer)? {
        serde_json::Value::Number(n) => n.as_i64(),
        serde_json::Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    })
}

/// Now, in unix seconds — the clock [`StoredTokens::expires_at`] is measured on.
#[must_use]
pub fn unix_now() -> i64 {
    now()
}

/// The process's broker.
///
/// See the module docs: the "one refresh at a time" invariant is per Keychain
/// item, not per object, so two brokers over the same store would race each
/// other exactly as two un-serialized callers would. Everything that
/// authenticates an OAuth account — the IMAP client, the SMTP sender and
/// `AccountService` — therefore goes through this one rather than each holding
/// its own.
static BROKER: std::sync::OnceLock<Arc<OAuthBroker>> = std::sync::OnceLock::new();

/// Install the process's broker. The first call wins.
///
/// Returns whether this call installed it. `rmaild` calls this at start-up;
/// tests call it to substitute a [`MemoryTokenStore`] for the Keychain, which
/// does not exist off macOS.
pub fn install_broker(broker: Arc<OAuthBroker>) -> bool {
    BROKER.set(broker).is_ok()
}

/// The process's broker, defaulting to a Keychain-backed one.
///
/// # Errors
///
/// [`Error::FailedPrecondition`] if no broker was installed and a default one
/// cannot be built.
pub fn broker() -> Result<Arc<OAuthBroker>, Error> {
    if let Some(installed) = BROKER.get() {
        return Ok(Arc::clone(installed));
    }
    let built = Arc::new(OAuthBroker::new(Arc::new(KeychainTokenStore))?);
    // A losing race installs nothing and uses whatever the winner installed,
    // so the "exactly one broker" invariant holds even without a lock.
    let _ = BROKER.set(Arc::clone(&built));
    BROKER.get().map_or(Ok(built), |b| Ok(Arc::clone(b)))
}

/// The Keychain service name an account's OAuth grant lives under, and the
/// broker key addressing it.
///
/// # Errors
///
/// [`Error::FailedPrecondition`] if the account is not an OAuth account or has
/// no username — the username is both the Keychain account field and the
/// `user=` of the XOAUTH2 string, so there is nothing to look up without it.
pub fn key_for(account: &crate::account::Account) -> Result<StoreKey, Error> {
    let crate::credential::CredentialSource::OAuth(service) = &account.credential else {
        return Err(Error::failed_precondition(format!(
            "account {} does not use OAuth2",
            account.id
        )));
    };
    let username = account
        .username
        .as_deref()
        .filter(|u| !u.is_empty())
        .ok_or_else(|| {
            Error::failed_precondition(format!(
                "account {} has no username; an OAuth grant is stored per login",
                account.id
            ))
        })?;
    Ok(StoreKey::new(service.clone(), username))
}

/// The SASL XOAUTH2 initial client response, as Google and Microsoft define it.
///
/// `user=<email>^Aauth=Bearer <token>^A^A`, where `^A` is `0x01`. IMAP and SMTP
/// share this exact string; only the base64 wrapping differs by transport,
/// which is why this returns the raw bytes and leaves encoding to the caller
/// (`async-imap` base64s the authenticator's response itself, and `lettre`
/// builds its own from the same two fields).
///
/// The result is a [`Secret`]: it contains the access token verbatim, so a
/// `Debug` of it anywhere near a connection log would print the bearer token.
#[must_use]
pub fn xoauth2(user: &str, access_token: &str) -> Secret {
    Secret::new(format!("user={user}\x01auth=Bearer {access_token}\x01\x01"))
}

/// The base64 form, for a SASL initial response sent inline.
#[must_use]
pub fn xoauth2_b64(user: &str, access_token: &str) -> Secret {
    use base64::Engine as _;
    Secret::new(
        base64::engine::general_purpose::STANDARD.encode(xoauth2(user, access_token).expose()),
    )
}
