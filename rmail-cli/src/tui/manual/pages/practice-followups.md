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
{{capability:SendSchedulerDismissFollowup}} clears one by hand. The 3d in
that line is the default spelled out — leave --in off and the reminder is due
in three days anyway. A reminder also cancels itself when a reply arrives, so
the common case needs no maintenance at all.

## Let it judge the deadline

{{capability:SendSchedulerTrackFollowup}} judges a sent message and picks the
delay; claude-haiku-4-5 is the model that does it. Nothing is spent until you
ask for that, and a judged deadline is clamped into four hours at the soonest
and, out of the box, thirty days at the latest, so a judge that answers
"three hundred days" cannot arm a reminder nobody will ever see. A judge that
says a reply is expected and then names no deadline at all lands on the same
three days --in would have given you.

## And the other direction

{{capability:SendSchedulerListWaitingOn}} is the list of threads where the
ball is with somebody else. It is the query that makes the follow-ups worth
arming, because a reminder you never look at is a reminder you did not need.

## Where the three days comes from

The send.followup table in the [[config-file]] holds every value this habit
leans on: default_delay is 3d, cancel_on_reply is true, model is
claude-haiku-4-5, and max_delay — the ceiling on a judged deadline — is 30d.
Each takes an environment override with the usual shape,
RMAIL_SEND__FOLLOWUP__DEFAULT_DELAY, so a week-long default does not need a
file edit. Per message, --in overrides the delay and --no-cancel-on-reply
overrides the cancelling.

The four-hour floor is the one number that is not a key: it lives in the
tracker, because a model answering "zero days" means urgent, not "nudge them
before they have opened it". Set max_delay under four hours and the ceiling
wins anyway — that number is one you chose deliberately, the floor is only
this tracker's guess about politeness.
