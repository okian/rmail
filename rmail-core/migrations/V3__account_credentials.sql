-- V3: credential references on accounts.
--
-- Accounts store *how* to resolve their password, never the password itself:
--   secret_kind = 'none' | 'command' | 'env' | 'keychain'
--   secret_ref  = the shell command / env var name / keychain service name
-- Resolution happens lazily at use; the plaintext secret is never persisted.
ALTER TABLE accounts ADD COLUMN secret_kind TEXT NOT NULL DEFAULT 'none';
ALTER TABLE accounts ADD COLUMN secret_ref TEXT;
