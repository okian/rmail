//! Row types for the tags subsystem: `tags`, `message_tags`, and the small
//! closed enums their columns are constrained to (migration V24).

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use rusqlite::Row;

use crate::config::TagSyncMode;

/// A persisted tag.
#[derive(Debug, Clone, PartialEq)]
pub struct Tag {
    /// Stable primary key.
    pub id: i64,
    /// Owning account (tags are per-account, like mailboxes).
    pub account_id: i64,
    /// The full hierarchical path (`"project/alpha"`), not just the leaf
    /// segment -- see [`super::hierarchy`] for why.
    pub name: String,
    /// Immediate parent tag, or `None` for a top-level tag.
    pub parent_id: Option<i64>,
    /// Display color (a palette entry or a truecolor hex string).
    pub color: Option<String>,
    /// `local`/`imap`/`auto` -- see [`super::sync`].
    pub sync_mode: TagSyncMode,
    /// Explicit wire keyword/label override; `None` means derive one from
    /// `tags.imap.keyword_prefix` + [`Tag::name`] -- see
    /// [`Tag::wire_keyword`].
    pub imap_keyword: Option<String>,
    /// Creation time (unix seconds).
    pub created_at: i64,
}

impl Tag {
    pub(crate) fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            account_id: row.get("account_id")?,
            name: row.get("name")?,
            parent_id: row.get("parent_id")?,
            color: row.get("color")?,
            sync_mode: row.get("sync_mode")?,
            imap_keyword: row.get("imap_keyword")?,
            created_at: row.get("created_at")?,
        })
    }

    /// The keyword/label this tag round-trips to on the wire: the explicit
    /// override if one was set, otherwise `keyword_prefix` + this tag's own
    /// name (`tags.imap.keyword_prefix`, prd.md's `"rmail/"` default).
    #[must_use]
    pub fn wire_keyword(&self, keyword_prefix: &str) -> String {
        self.imap_keyword
            .clone()
            .unwrap_or_else(|| format!("{keyword_prefix}{}", self.name))
    }
}

/// [`Tag`] plus how many messages currently carry it effectively (its own
/// applications, or its thread's -- see `messages_tags_effective`) -- the
/// `mail tags` / `ListTags` view (prd.md CLI: "list all + counts").
#[derive(Debug, Clone, PartialEq)]
pub struct TagWithCount {
    /// The tag itself.
    pub tag: Tag,
    /// Distinct messages this tag is effectively applied to.
    pub message_count: i64,
}

/// Who or what applied a `message_tags` row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagSource {
    /// A person, via `mail tag`/`AddTag`/`BulkTag`.
    User,
    /// Claude, via the suggestion pipeline (task 57) -- always paired with
    /// [`TagState::Pending`] until [`super::TagStore::resolve_suggestion`]
    /// runs.
    Ai,
    /// A deterministic `tag_rules` match (task 57/66).
    Rule,
    /// Imported from a server-side IMAP keyword/Gmail label -- see
    /// [`super::TagStore::import_imap_keywords`].
    Imap,
}

impl TagSource {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Ai => "ai",
            Self::Rule => "rule",
            Self::Imap => "imap",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "user" => Some(Self::User),
            "ai" => Some(Self::Ai),
            "rule" => Some(Self::Rule),
            "imap" => Some(Self::Imap),
            _ => None,
        }
    }
}

impl ToSql for TagSource {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_str()))
    }
}

impl FromSql for TagSource {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let text = value.as_str()?;
        Self::parse(text).ok_or_else(|| FromSqlError::Other(unknown_variant("source", text)))
    }
}

/// A `message_tags` row's lifecycle state -- see migration V24's own
/// comments for what each means and who may see it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagState {
    /// A real, visible tag -- counted in `messages_tags_effective`, matched
    /// by `tag:`/`has:tag`, rendered as a chip.
    Applied,
    /// An AI suggestion awaiting [`super::TagStore::resolve_suggestion`].
    Pending,
    /// A resolved-no. Kept (not deleted) so a future suggestion pass can
    /// learn from it rather than re-suggesting blindly.
    Rejected,
}

impl TagState {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::Pending => "pending",
            Self::Rejected => "rejected",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "applied" => Some(Self::Applied),
            "pending" => Some(Self::Pending),
            "rejected" => Some(Self::Rejected),
            _ => None,
        }
    }
}

impl ToSql for TagState {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_str()))
    }
}

impl FromSql for TagState {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let text = value.as_str()?;
        Self::parse(text).ok_or_else(|| FromSqlError::Other(unknown_variant("state", text)))
    }
}

/// [`rusqlite`]'s [`ToSql`]/[`FromSql`] for [`crate::config::TagSyncMode`]
/// live here rather than in [`crate::config`]: that module has no other
/// reason to depend on `rusqlite` (it is a figment/serde-only settings
/// layer), and implementing a foreign trait ([`ToSql`]/[`FromSql`], from the
/// `rusqlite` crate) for a local type ([`TagSyncMode`], defined in this same
/// crate) is legal from any module in the crate under Rust's orphan rules --
/// there is no requirement that the impl sit next to the type definition,
/// only that at least one of the two is local. Reusing the config enum
/// (rather than a second, identically-shaped domain enum) means a tag's
/// persisted `sync_mode` and `tags.default_sync_mode` are provably the same
/// three values, not two enums a future edit could let drift apart.
impl ToSql for TagSyncMode {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        let text = match self {
            TagSyncMode::Local => "local",
            TagSyncMode::Imap => "imap",
            TagSyncMode::Auto => "auto",
        };
        Ok(ToSqlOutput::from(text))
    }
}

impl FromSql for TagSyncMode {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let text = value.as_str()?;
        match text {
            "local" => Ok(TagSyncMode::Local),
            "imap" => Ok(TagSyncMode::Imap),
            "auto" => Ok(TagSyncMode::Auto),
            other => Err(FromSqlError::Other(unknown_variant("sync_mode", other))),
        }
    }
}

fn unknown_variant(column: &str, value: &str) -> Box<dyn std::error::Error + Send + Sync> {
    format!("unrecognized {column} value {value:?} (migration V24's CHECK should have rejected it)")
        .into()
}

/// Which side of `message_tags`' `CHECK ((message_id IS NULL) <>
/// (thread_id IS NULL))` a row/request is on -- the same distinction
/// prd.md's own `message Target { oneof { message_id; thread_id } }` makes
/// at the proto layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// A single message.
    Message(i64),
    /// A whole thread -- current *and future* members, via
    /// `messages_tags_effective`'s join on `thread_id`.
    Thread(i64),
}

/// A persisted `message_tags` row.
#[derive(Debug, Clone, PartialEq)]
pub struct MessageTag {
    /// Stable primary key.
    pub id: i64,
    /// The tag applied.
    pub tag_id: i64,
    /// What this row is attached to.
    pub target: Target,
    /// Who/what applied it.
    pub source: TagSource,
    /// Its lifecycle state.
    pub state: TagState,
    /// AI confidence, `source = Ai` only.
    pub confidence: Option<f64>,
    /// AI rationale, `source = Ai` only.
    pub rationale: Option<String>,
    /// Creation time (unix seconds).
    pub created_at: i64,
}

impl MessageTag {
    pub(crate) fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        let message_id: Option<i64> = row.get("message_id")?;
        let thread_id: Option<i64> = row.get("thread_id")?;
        // The `CHECK` constraint guarantees exactly one is set; `unwrap_or`
        // rather than a panic-on-violation assert keeps this fallible path
        // (`rusqlite::Result`) rather than introducing a `panic!` in
        // non-test code over an invariant the schema already enforces.
        let target = match (message_id, thread_id) {
            (Some(id), None) => Target::Message(id),
            (None, Some(id)) => Target::Thread(id),
            _ => Target::Message(message_id.or(thread_id).unwrap_or(0)),
        };
        Ok(Self {
            id: row.get("id")?,
            tag_id: row.get("tag_id")?,
            target,
            source: row.get("source")?,
            state: row.get("state")?,
            confidence: row.get("confidence")?,
            rationale: row.get("rationale")?,
            created_at: row.get("created_at")?,
        })
    }
}

/// Fields required to insert a new `message_tags` row.
#[derive(Debug, Clone)]
pub struct NewMessageTag {
    /// The tag being applied.
    pub tag_id: i64,
    /// What it is being applied to.
    pub target: Target,
    /// Who/what is applying it.
    pub source: TagSource,
    /// Initial lifecycle state (`Applied` for a direct user/rule/imap
    /// application, `Pending` for an AI suggestion).
    pub state: TagState,
    /// AI confidence, if `source = Ai`.
    pub confidence: Option<f64>,
    /// AI rationale, if `source = Ai`.
    pub rationale: Option<String>,
}

/// One pending AI suggestion, joined with the [`Tag`] it names -- what
/// [`super::TagStore::list_pending_suggestions`] (`SuggestTags`'s backing
/// read) hands back.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingSuggestion {
    /// The `message_tags` row (`state = Pending`).
    pub message_tag: MessageTag,
    /// The tag it names.
    pub tag: Tag,
}
