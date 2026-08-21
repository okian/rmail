use super::*;

#[test]
fn a_pushed_entry_pops_back_out_unchanged() {
    let mut stack = Stack::default();
    let entry = Entry::mv(10, 2);
    stack.push(entry.clone());
    assert_eq!(stack.pop(), Some(entry));
}

#[test]
fn pushing_past_max_entries_evicts_the_oldest_not_the_newest() {
    let mut stack = Stack::default();
    for id in 0..MAX_ENTRIES {
        stack.push(Entry::mv(id.try_into().unwrap(), 1));
    }
    // One more push should evict message_id 0 (the oldest) — not touch the
    // newest, and not simply refuse the new one.
    stack.push(Entry::mv(9999, 1));

    let mut popped = Vec::new();
    while let Some(entry) = stack.pop() {
        let Entry::Move { message_id, .. } = entry else {
            unreachable!()
        };
        popped.push(message_id);
    }
    assert_eq!(popped.len(), MAX_ENTRIES, "still capped, not grown past it");
    assert_eq!(popped[0], 9999, "the newest push is still on top");
    assert!(
        !popped.contains(&0),
        "message_id 0 was the oldest and should have been evicted"
    );
    assert!(
        popped.contains(&1),
        "message_id 1 should have survived — only the single oldest was evicted"
    );
}

#[test]
fn pop_on_an_empty_stack_is_none() {
    let mut stack = Stack::default();
    assert_eq!(stack.pop(), None);
}

#[test]
fn pop_is_lifo() {
    let mut stack = Stack::default();
    let a = Entry::mv(1, 2);
    let b = Entry::flags(3, vec!["\\Seen".to_owned()]);
    let c = Entry::tag(4, "invoices".to_owned(), true);
    stack.push(a.clone());
    stack.push(b.clone());
    stack.push(c.clone());
    assert_eq!(stack.pop(), Some(c), "the most recently pushed comes first");
    assert_eq!(stack.pop(), Some(b));
    assert_eq!(stack.pop(), Some(a));
    assert_eq!(stack.pop(), None, "and then it is empty again");
}

#[test]
fn popping_an_entry_removes_it_so_it_cannot_be_popped_twice() {
    // The structural half of "a retried undo cannot double-apply": once an
    // entry is popped it is gone from the stack, full stop — there is no
    // way to pop the same physical entry a second time.
    let mut stack = Stack::default();
    stack.push(Entry::mv(1, 2));
    assert!(stack.pop().is_some());
    assert_eq!(stack.pop(), None);
}

#[test]
fn is_empty_reflects_pushes_and_pops() {
    let mut stack = Stack::default();
    assert!(stack.is_empty());
    stack.push(Entry::mv(1, 2));
    assert!(!stack.is_empty());
    stack.pop();
    assert!(stack.is_empty());
}

#[test]
fn every_constructor_mints_a_non_empty_key() {
    let entries = [
        Entry::mv(1, 2),
        Entry::flags(1, Vec::new()),
        Entry::tag(1, "x".to_owned(), false),
        Entry::cancel_scheduled(9),
    ];
    for entry in entries {
        let key = match &entry {
            Entry::Move {
                idempotency_key, ..
            }
            | Entry::Flags {
                idempotency_key, ..
            }
            | Entry::Tag {
                idempotency_key, ..
            }
            | Entry::CancelScheduled {
                idempotency_key, ..
            } => idempotency_key,
        };
        assert!(!key.is_empty(), "{entry:?}");
    }
}

#[test]
fn two_entries_of_the_same_kind_get_different_keys() {
    // Freshly minted per construction, not derived from the message id or
    // any other field that two otherwise-identical entries would share.
    let Entry::Move {
        idempotency_key: first,
        ..
    } = Entry::mv(1, 2)
    else {
        unreachable!()
    };
    let Entry::Move {
        idempotency_key: second,
        ..
    } = Entry::mv(1, 2)
    else {
        unreachable!()
    };
    assert_ne!(first, second);
}

#[test]
fn a_minted_key_looks_like_a_uuid() {
    // Not a format guarantee this module promises callers, just a sanity
    // check that `new_key` is really drawing from `Uuid::new_v4` and not
    // some placeholder — prd.md's own spec for these is "(UUID)".
    let Entry::Move {
        idempotency_key, ..
    } = Entry::mv(1, 2)
    else {
        unreachable!()
    };
    assert!(
        uuid::Uuid::parse_str(&idempotency_key).is_ok(),
        "{idempotency_key}"
    );
}

#[test]
fn tags_own_direction_is_exactly_what_the_caller_passed() {
    let Entry::Tag { remove, .. } = Entry::tag(1, "x".to_owned(), true) else {
        unreachable!()
    };
    assert!(remove);
    let Entry::Tag { remove, .. } = Entry::tag(1, "x".to_owned(), false) else {
        unreachable!()
    };
    assert!(!remove);
}
