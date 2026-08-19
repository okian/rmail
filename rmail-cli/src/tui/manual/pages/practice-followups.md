# Practice: arm the reminder when you send

Create the follow-up in the same breath as the message it is about, not on
the day you start wondering whether anybody replied.

## Why

At send time you still know what you are waiting for and when it stops being
reasonable; a week later you know neither, which is why the reminder never
gets created.

## The one line

```
mail followup add --account 1 --in 3d --note "chase quote" <message-id>
```

{{capability:SendSchedulerCreateFollowup}} arms it,
{{capability:SendSchedulerListFollowups}} lists what is armed, and
{{capability:SendSchedulerDismissFollowup}} clears one by hand. By default a
reminder cancels itself when a reply arrives, so the common case needs no
maintenance at all.

## Let it judge the deadline

{{capability:SendSchedulerTrackFollowup}} judges a sent message and picks the
delay. Nothing is spent until you ask for that, and a judged deadline is
clamped into a sane range, so a judge that answers "three hundred days" cannot
arm a reminder nobody will ever see.

## And the other direction

{{capability:SendSchedulerListWaitingOn}} is the list of threads where the
ball is with somebody else. It is the query that makes the follow-ups worth
arming, because a reminder you never look at is a reminder you did not need.
