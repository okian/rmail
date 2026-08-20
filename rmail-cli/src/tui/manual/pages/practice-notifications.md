# Practice: raise the threshold until it is quiet

Set the notification threshold so that a normal day produces no notifications
at all, then lower it only if something genuinely urgent got through silently.

## Why

A notifier that fires on ordinary mail is a notifier you turn off, and a
notifier you turned off is worse than none because you think you have one.

## The knobs that matter

```
[notify]
enabled = false
threshold = "high"
max_message_age = "60m"

[notify.quiet_hours]
enabled = false
start = "22:00"
end = "07:00"
```

That is the table as it ships, in the [[config-file]]. Notifications are off
by default, and notify.enabled is the one line you write to start paying a
model call per synced message. The age bound is the one people forget: a sync
that catches up on a day of mail should not produce a day of alerts at once,
and this is what stops it — anything that arrived more than 60m ago is
declined before the model call, and declined for good, since a later attempt
cannot make an old message young. Quiet hours are off out of the box, and once
you turn them on a notification that comes due inside the window is held until
the window closes rather than dropped.

## What the threshold is measured against

{{capability:NotificationScoreMessage}} scores a message rather than matching
a rule, so the threshold is a judgement about importance and not a filter on
senders. The ladder is low, normal, high and critical, and high is the
default; a value outside that vocabulary delivers nothing at all rather than
falling back to a tier, so the way to switch notifications off is
notify.enabled and never a made-up threshold. Per-account thresholds exist for
the same reason accounts do — the line is different for different mail. An
account's own notify.enabled and notify.threshold are unset rather than
defaulted, so the notify table answers until one of them says otherwise, and
raising the global line moves every account you never touched with it. See
[[practice-accounts]].

## And what the alert says

Including the subject is a default; including the reason is not, because the
reason is derived from content and a lock screen is a surface you do not
control. The channel is auto, which means the local desktop notifier and
nothing else: there is deliberately no webhook or push variant, so turning
notifications on does not make mail leave the machine.

## What to check when nothing fired

Ask the daemon rather than re-reading the file.

```
mail notify score <id>   what this daemon decided about one message
mail notify watch        follow alerts as they fire
```

score prints the tier and the model's one-line reason, the threshold in force
for that message's account after the per-account override, whether that
account notifies at all, and whether the tier would have pinged; a suppressed
line names which gate closed, below_threshold or notifications_disabled. On a
message it never scored it queues a scoring pass instead, and with
notify.enabled false it refuses outright rather than scoring, because an RPC
is not a way around a switch you turned off.
{{capability:NotificationStreamAlerts}} is what watch follows, and a day of it
with nothing in it is the state this page is asking for.

Every field in the notify table takes an environment override of the same
shape — RMAIL_NOTIFY__ENABLED, RMAIL_NOTIFY__THRESHOLD — so trying a stricter
line for one run needs no edit. The per-account pair has no environment
spelling at all, because accounts are an array of tables; that one is a file
edit.
