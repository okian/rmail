-- V1: foundational schema.
--
-- Only the metadata table lives here; the domain schema (accounts, mailboxes,
-- messages, ...) arrives in task 6 as V2+. This exists so the migration runner
-- has something to apply from day one and so app/schema metadata has a home.
CREATE TABLE _rmail_meta (
    key   TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL
);
