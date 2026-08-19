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
  command or an environment variable, lazily and never persisted in plaintext.
- Notification thresholds, which are per account for the same reason.
- The archive folder, which is a per-server convention. See [[archive]].

## And what does not

Search, tags and the finder are unified across accounts by default, because
the reason you split the accounts was the policy, not the retrieval.
{{capability:MailListUnified}} is the unified inbox that follows from the
same choice.
