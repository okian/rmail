# Practice: raise the threshold until it is quiet

Set the notification threshold so that a normal day produces no notifications
at all, then lower it only if something genuinely urgent got through silently.

## Why

A notifier that fires on ordinary mail is a notifier you turn off, and a
notifier you turned off is worse than none because you think you have one.

## The knobs that matter

```
[notify]
enabled = true
threshold = "high"
max_message_age = "60m"

[notify.quiet_hours]
enabled = true
start = "22:00"
end = "07:00"
```

Notifications are off by default. The age bound is the one people forget: a
sync that catches up on a day of mail should not produce a day of alerts at
once, and this is what stops it.

## What the threshold is measured against

{{capability:NotificationScoreMessage}} scores a message rather than matching
a rule, so the threshold is a judgement about importance and not a filter on
senders. Per-account thresholds exist for the same reason accounts do — the
line is different for different mail. See [[practice-accounts]].

## And what the alert says

Including the subject is a default; including the reason is not, because the
reason is derived from content and a lock screen is a surface you do not
control.
