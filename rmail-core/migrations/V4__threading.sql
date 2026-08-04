-- V4: threading.
--
-- Conversations are keyed on the RFC5322 reference graph. `thread_refs` maps
-- every message-id an account has *seen or been told about* -- including
-- "phantom" ids that a reply references but whose message has not been fetched
-- -- to a thread. That phantom registration is what makes out-of-order arrival
-- stable: a reply seen first registers its parent's id, so when the parent
-- lands later it joins the existing thread instead of starting a new one, and
-- the thread id never changes underneath a client.
--
-- Note `message_id` here is the RFC822 header value (angle brackets stripped),
-- NOT `messages.id` -- phantom rows have no message row at all.
CREATE TABLE thread_refs (
    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    message_id TEXT NOT NULL,
    thread_id  INTEGER NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    PRIMARY KEY (account_id, message_id)
) STRICT;

-- Merging two threads repoints every ref of the losing thread; that scan is by
-- thread_id, which the primary key does not cover.
CREATE INDEX idx_thread_refs_thread ON thread_refs(thread_id);

-- Derived participant set: distinct addresses across the thread's messages,
-- lowercased, sorted, comma-joined. (Addresses with a quoted local part may
-- legally contain a comma; that is rare enough to accept a split artifact
-- rather than pay for escaping here.)
ALTER TABLE threads ADD COLUMN participants TEXT;

-- Timestamp of the thread's *earliest* message. The subject-normalization
-- fallback is anchored on this, not on last_message_at: last_message_at
-- advances with every arrival, so a window measured against it slides forward
-- forever and unrelated mail sharing a generic subject ("Invoice") accretes
-- into one endless thread.
ALTER TABLE threads ADD COLUMN first_message_at INTEGER;

-- The subject fallback seeks same-subject threads in an account.
CREATE INDEX idx_threads_subject_norm
    ON threads(account_id, subject_norm, last_message_at DESC);

-- The conversation-list page: an account's threads, most recent first.
-- idx_threads_last_message has no account_id, so it cannot serve this.
CREATE INDEX idx_threads_account_activity
    ON threads(account_id, last_message_at DESC);
