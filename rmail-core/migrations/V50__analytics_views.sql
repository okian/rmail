-- V50: the read-only analytics surface (task 72, prd.md feature 61).
--
-- `AnalyticsService.AskAnalytics` lets Claude write SQL. These views are the
-- only schema it is ever shown and the only schema it is ever allowed to
-- touch: `analytics::sql` installs a SQLite authorizer that denies every
-- action except a read reaching a table *through* one of the names below, so
-- a model that emits `SELECT token FROM api_tokens` fails at prepare time
-- rather than at review time.
--
-- # Why views rather than a column allow-list over the base tables
--
-- Three reasons, and each of them on its own would be enough:
--
-- 1. **Bodies and secrets are not in the surface at all.** `messages.raw`,
--    `body_text`, `body_html`, `account_credentials`, `api_tokens` and the AI
--    ledger are not projected by any view here, so "did the model ask for a
--    body" is not a question the guard has to get right — there is no name it
--    could spell to reach one.
-- 2. **The joins are pre-decided.** Flag lookups, the account name and the
--    folder name are already resolved, so a question about unread mail does
--    not depend on the model rediscovering that read-ness lives in a separate
--    `flags` row keyed on `\Seen`.
-- 3. **The prompt is small and stable.** A schema of six views with plain
--    column names fits in a cached system prompt; the base schema does not,
--    and would have to grow every time a feature table lands.
--
-- # `direction` is a heuristic, and says so
--
-- A message is `outbound` when it sits in a folder whose name reads as Sent,
-- or when its `From` matches the account's configured username. That is the
-- SQL-expressible half of what `analytics::response_time::self_addresses`
-- computes (which additionally unions every distinct sender ever seen in a
-- Sent folder, and cannot be a view because it needs a cap). An account with
-- aliases and no Sent folder therefore under-counts outbound here. The column
-- comment says so, and the prompt repeats it, because a number a model will
-- narrate has to carry its own caveat.
--
-- # `account_id` is a filter, not a boundary
--
-- Every view carries `account_id` and the prompt tells the model to filter on
-- it when the caller scoped the question. That is a convenience, and it is
-- deliberately not described as anything stronger: `mail.read` is not a
-- per-account scope anywhere in this daemon (`GetResponseTimes` with
-- `account_id = 0` already reports on every account), so a caller who can ask
-- one account's question can ask them all. Claiming otherwise here would be
-- inventing a boundary the auth layer does not have.

-- One row per message row in the local mirror, minus everything that is not a
-- fact about the message. No body, no raw octets, no attachment bytes.
--
-- `sent_at` is `COALESCE(date, internaldate)` -- the same clock every other
-- report in this build sorts and windows on, so a question asked here and the
-- same question asked through `mail search` agree about which day a message
-- belongs to.
CREATE VIEW analytics_messages AS
SELECT
    m.id                                    AS message_id,
    m.account_id                            AS account_id,
    a.name                                  AS account_name,
    m.mailbox_id                            AS mailbox_id,
    b.name                                  AS mailbox,
    m.thread_id                             AS thread_id,
    lower(trim(m.from_addr))                AS from_addr,
    m.from_name                             AS from_name,
    m.to_addrs                              AS to_addrs,
    m.cc_addrs                              AS cc_addrs,
    m.subject                               AS subject,
    COALESCE(m.date, m.internaldate)        AS sent_at,
    date(COALESCE(m.date, m.internaldate), 'unixepoch') AS sent_day,
    m.size                                  AS size_bytes,
    m.has_attachments                       AS has_attachments,
    CASE WHEN EXISTS (
        SELECT 1 FROM flags f WHERE f.message_id = m.id AND f.flag = '\Seen'
    ) THEN 1 ELSE 0 END                     AS is_read,
    CASE WHEN EXISTS (
        SELECT 1 FROM flags f WHERE f.message_id = m.id AND f.flag = '\Flagged'
    ) THEN 1 ELSE 0 END                     AS is_flagged,
    CASE WHEN EXISTS (
        SELECT 1 FROM flags f WHERE f.message_id = m.id AND f.flag = '\Answered'
    ) THEN 1 ELSE 0 END                     AS is_answered,
    -- See the header on why this is a heuristic.
    CASE
        WHEN lower(b.name) IN (
            'sent', 'sent items', 'sent messages', 'sent mail',
            'inbox.sent', '[gmail]/sent mail'
        ) THEN 'outbound'
        WHEN lower(trim(m.from_addr)) = lower(trim(a.username)) THEN 'outbound'
        ELSE 'inbound'
    END                                     AS direction
FROM messages m
JOIN mailboxes b ON b.id = m.mailbox_id
JOIN accounts  a ON a.id = m.account_id;

-- One row per (account, sender). The shape most "who sends me the most / who
-- do I never read" questions want, pre-aggregated so a model does not have to
-- re-derive the read-rate join every time.
CREATE VIEW analytics_senders AS
SELECT
    account_id                              AS account_id,
    from_addr                               AS from_addr,
    MAX(from_name)                          AS from_name,
    COUNT(*)                                AS messages,
    SUM(is_read)                            AS read_messages,
    -- Guarded against the empty group even though GROUP BY cannot produce
    -- one: a later edit that turns this into a LEFT JOIN would otherwise
    -- divide by zero silently, and SQLite returns NULL rather than erroring.
    CAST(SUM(is_read) AS REAL) / MAX(COUNT(*), 1) AS read_rate,
    MIN(sent_at)                            AS first_seen,
    MAX(sent_at)                            AS last_seen,
    COUNT(DISTINCT thread_id)               AS threads
FROM analytics_messages
WHERE direction = 'inbound' AND from_addr IS NOT NULL
GROUP BY account_id, from_addr;

-- One row per (account, day, direction): the volume series.
CREATE VIEW analytics_daily AS
SELECT
    account_id                              AS account_id,
    sent_day                                AS sent_day,
    direction                               AS direction,
    COUNT(*)                                AS messages,
    SUM(is_read)                            AS read_messages
FROM analytics_messages
WHERE sent_day IS NOT NULL
GROUP BY account_id, sent_day, direction;

-- One row per thread, with the counts a "which conversations are long /
-- stale" question needs.
CREATE VIEW analytics_threads AS
SELECT
    t.id                                    AS thread_id,
    t.account_id                            AS account_id,
    t.subject_norm                          AS subject,
    t.message_count                         AS messages,
    t.last_message_at                       AS last_message_at,
    (SELECT COUNT(*) FROM analytics_messages m
      WHERE m.thread_id = t.id AND m.direction = 'outbound') AS outbound_messages,
    (SELECT COUNT(*) FROM analytics_messages m
      WHERE m.thread_id = t.id AND m.direction = 'inbound')  AS inbound_messages
FROM threads t;

-- One row per folder.
CREATE VIEW analytics_mailboxes AS
SELECT
    b.id                                    AS mailbox_id,
    b.account_id                            AS account_id,
    b.name                                  AS mailbox,
    (SELECT COUNT(*) FROM analytics_messages m WHERE m.mailbox_id = b.id) AS messages,
    (SELECT COUNT(*) FROM analytics_messages m
      WHERE m.mailbox_id = b.id AND m.is_read = 0)          AS unread_messages
FROM mailboxes b;

-- The derived contact graph, unchanged except for being renamed into the
-- analytics namespace so the guard has exactly one prefix to reason about.
CREATE VIEW analytics_contacts AS
SELECT
    c.address                               AS address,
    c.name                                  AS name,
    c.message_count                          AS messages,
    c.last_seen                             AS last_seen
FROM contacts c;
