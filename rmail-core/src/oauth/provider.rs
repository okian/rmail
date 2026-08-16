//! The two providers rmail brokers tokens for, and their endpoints.

use crate::error::Error;

use super::url::encode_query;

/// An OAuth2 provider rmail can authorize against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Provider {
    /// Google (Gmail).
    Google,
    /// Microsoft identity platform (Outlook / Office 365).
    Microsoft,
}

impl Provider {
    /// Every provider, for enumeration in help text and tests.
    pub const ALL: &'static [Provider] = &[Provider::Google, Provider::Microsoft];

    /// Parse a provider name as a user would type it.
    ///
    /// Both the vendor name and the mail brand are accepted, because the
    /// account being added is a "gmail" or an "outlook" account long before
    /// anybody thinks of it as a Google or Microsoft one.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgument`] for an unknown name.
    pub fn parse(name: &str) -> Result<Self, Error> {
        match name.trim().to_ascii_lowercase().as_str() {
            "google" | "gmail" => Ok(Self::Google),
            "microsoft" | "outlook" | "office365" | "o365" => Ok(Self::Microsoft),
            other => Err(Error::invalid_argument(format!(
                "unknown OAuth provider {other:?} (use google/gmail or microsoft/outlook)"
            ))),
        }
    }

    /// The canonical name, as persisted and as accepted by [`Provider::parse`].
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Google => "google",
            Self::Microsoft => "microsoft",
        }
    }

    /// The vendor name, for messages a user reads.
    #[must_use]
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Google => "Google",
            Self::Microsoft => "Microsoft",
        }
    }

    /// The authorization endpoint the user's browser is sent to.
    #[must_use]
    pub const fn authorize_endpoint(self) -> &'static str {
        match self {
            Self::Google => "https://accounts.google.com/o/oauth2/v2/auth",
            // The `common` tenant covers both work/school and personal
            // accounts, which is the only choice that works for a client that
            // does not know which kind it is being pointed at.
            Self::Microsoft => "https://login.microsoftonline.com/common/oauth2/v2.0/authorize",
        }
    }

    /// The token endpoint codes and refresh tokens are exchanged at.
    #[must_use]
    pub const fn token_endpoint(self) -> &'static str {
        match self {
            Self::Google => "https://oauth2.googleapis.com/token",
            Self::Microsoft => "https://login.microsoftonline.com/common/oauth2/v2.0/token",
        }
    }

    /// The scopes requested when a caller names none.
    ///
    /// Deliberately the narrowest set that lets rmail do its job: read and
    /// modify mail, and send it. Neither list asks for profile or contact
    /// data. Google's `https://mail.google.com/` is coarser than anyone would
    /// like, but it is the only Google scope that grants IMAP and SMTP access
    /// — the finer `gmail.readonly`/`gmail.modify` scopes work over the REST
    /// API only, and an IMAP `AUTHENTICATE XOAUTH2` with them fails.
    #[must_use]
    pub fn default_scopes(self) -> Vec<String> {
        match self {
            Self::Google => vec!["https://mail.google.com/".to_owned()],
            // `offline_access` is what makes Microsoft issue a refresh token
            // at all; without it the grant dies with the access token.
            Self::Microsoft => vec![
                "offline_access".to_owned(),
                "https://outlook.office.com/IMAP.AccessAsUser.All".to_owned(),
                "https://outlook.office.com/SMTP.Send".to_owned(),
            ],
        }
    }

    /// The provider's IMAP endpoint, for autoconfiguring a new account.
    #[must_use]
    pub const fn imap_endpoint(self) -> (&'static str, u16) {
        match self {
            Self::Google => ("imap.gmail.com", 993),
            Self::Microsoft => ("outlook.office365.com", 993),
        }
    }

    /// The provider's SMTP submission endpoint.
    #[must_use]
    pub const fn smtp_endpoint(self) -> (&'static str, u16) {
        match self {
            Self::Google => ("smtp.gmail.com", 587),
            Self::Microsoft => ("smtp.office365.com", 587),
        }
    }

    /// The full authorization URL for a PKCE loopback flow.
    ///
    /// `access_type=offline` and `prompt=consent` are Google-specific and are
    /// what make it return a refresh token: without them a second
    /// authorization for an already-consented client returns an access token
    /// only, and the account can never sync again unattended. Microsoft gets
    /// the same guarantee from the `offline_access` scope.
    #[must_use]
    pub fn authorization_url(
        self,
        client_id: &str,
        redirect_uri: &str,
        scopes: &[String],
        state: &str,
        code_challenge: &str,
    ) -> String {
        let scope = scopes.join(" ");
        let mut params = vec![
            ("client_id", client_id),
            ("response_type", "code"),
            ("redirect_uri", redirect_uri),
            ("scope", scope.as_str()),
            ("state", state),
            ("code_challenge", code_challenge),
            ("code_challenge_method", "S256"),
        ];
        if self == Self::Google {
            params.push(("access_type", "offline"));
            params.push(("prompt", "consent"));
        }
        format!("{}?{}", self.authorize_endpoint(), encode_query(&params))
    }
}

impl std::fmt::Display for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
