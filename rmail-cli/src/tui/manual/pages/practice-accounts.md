# Practice: one account per trust boundary

Split mail into separate accounts along the line where the rules about it
change, not along the line where the mail comes from.

## Why

Account is the coarsest control rmail has and the only one that cannot be
misconfigured into leaking, because a per-account AI opt-out stops the calls
from being possible rather than from being made.

## The line that matters

```
[[accounts]]
name = "Personal-Legal"
[accounts.ai]
enabled = false
residency = "eu"
```

An account with AI disabled is not an account whose AI features are hidden.
Nothing enriches it, nothing embeds it, and no question can reach it. Compare
a folder-level policy rule, which is finer-grained and does the same job for
one folder — see [[privacy]].

## What else follows the boundary

- Credentials. Each account resolves its own, from the keychain, a password
  command, an environment variable or an OAuth grant, lazily and never
  persisted in plaintext.
- Notification thresholds, which are per account for the same reason.
- The archive folder, which is a per-server convention: the client archives
  into the first of Archive, Archives or All Mail the account reports, and
  says an account has none rather than failing the action. See [[archive]].

## And what does not

Search, tags and the finder are unified across accounts by default, because
the reason you split the accounts was the policy, not the retrieval.
{{capability:MailListUnified}} is the unified inbox that follows from the
same choice.

## What an account inherits until you say otherwise

Every line in the TOML above is an override. Out of the box
accounts.ai.enabled is true, so a new account permits AI processing from its
first sync, and the trust boundary is something you assert rather than
something you relax. An account that names no residency falls through to
ai.policy.default_residency, which is unspecified, under a default_mode of
allowed — [[privacy]] is the rule table that resolves it.

Per-account notify.enabled and notify.threshold are unset rather than
defaulted, and the difference is load-bearing: unset means this account did
not say, so the notify table answers for it — off entirely by default, and a
threshold of high once you turn it on. Raise the global threshold later and
the accounts you never touched move with it, which is only true because they
hold no value of their own. See [[practice-notifications]].

The two ports default as well: omit them and IMAP is 993 and SMTP 587. The
credential is the one field with no default at all — an account that names
no password_command, no password_env and no keychain entry, and has had no
OAuth grant attached to it, is created and cannot log in, which is a
legitimate intermediate state rather than an error. The first three are keys
you write in the file; an OAuth grant is not, because the flow attaches it
afterwards. Which of the four kinds a given provider will accept is the
provider's decision and not yours: [[provider-settings]] holds that table,
and [[add-any-account]] is the procedure that makes the account and its
credential in the first place.

accounts is an array of tables, so it has no environment spelling.
RMAIL_NOTIFY__THRESHOLD moves the global threshold for one run; a per-account
one is an edit to the [[config-file]].
