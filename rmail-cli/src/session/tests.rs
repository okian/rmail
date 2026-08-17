use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use super::*;

/// An in-process store, for tests only — mirrors
/// `rmail_core::oauth::store::MemoryTokenStore`'s role exactly: nothing
/// wires this up in `mail`, and it exists so the expiry check and the
/// round-trip through [`CachedSession::to_blob`]/`from_blob` can be
/// exercised without the real Keychain, which this workspace's Docker test
/// container (Linux) cannot reach at all.
#[derive(Default)]
struct MemoryStore {
    entries: Mutex<HashMap<String, String>>,
}

impl SessionStore for MemoryStore {
    fn load(&self, account: &str) -> Result<Option<CachedSession>, SessionStoreError> {
        Ok(self
            .entries
            .lock()
            .unwrap()
            .get(account)
            .and_then(|raw| CachedSession::from_blob(raw)))
    }

    fn save(&self, account: &str, session: &CachedSession) -> Result<(), SessionStoreError> {
        self.entries
            .lock()
            .unwrap()
            .insert(account.to_owned(), session.to_blob());
        Ok(())
    }

    fn clear(&self, account: &str) -> Result<(), SessionStoreError> {
        self.entries.lock().unwrap().remove(account);
        Ok(())
    }
}

fn session(token: &str, expires_at: i64) -> CachedSession {
    CachedSession {
        token: token.to_owned(),
        expires_at,
        token_id: 1,
    }
}

#[test]
fn a_session_round_trips_through_its_blob_encoding() {
    let s = session("rmail_tok_1_abc123", 1_700_000_000);
    let blob = s.to_blob();
    let decoded = CachedSession::from_blob(&blob).expect("should decode");
    assert_eq!(decoded.token, s.token);
    assert_eq!(decoded.expires_at, s.expires_at);
    assert_eq!(decoded.token_id, s.token_id);
}

#[test]
fn malformed_blobs_decode_to_none_rather_than_panicking() {
    for bad in [
        "",
        "no-tab-at-all",
        "\t1700000000\t1",
        "token\tnot-a-number\t1",
        "token\t1700000000\tnot-a-number",
        "token\t1700000000",          // missing token_id
        "token\t1700000000\t1\tjunk", // trailing extra field
    ] {
        assert!(
            CachedSession::from_blob(bad).is_none(),
            "{bad:?} should not decode"
        );
    }
}

#[test]
fn save_then_load_returns_the_same_session() {
    let store = MemoryStore::default();
    let socket = Path::new("/tmp/rmaild.sock");
    let s = session("rmail_tok_1_abc123", now_unix() + 3600);

    save_to(&store, socket, &s).unwrap();
    let loaded = load_from(&store, socket).expect("should be cached");
    assert_eq!(loaded.token, s.token);
    assert_eq!(loaded.expires_at, s.expires_at);
}

#[test]
fn an_expired_session_loads_as_none() {
    let store = MemoryStore::default();
    let socket = Path::new("/tmp/rmaild.sock");
    let s = session("rmail_tok_1_abc123", now_unix() - 1);

    save_to(&store, socket, &s).unwrap();
    assert!(load_from(&store, socket).is_none());
}

#[test]
fn nothing_cached_loads_as_none() {
    let store = MemoryStore::default();
    assert!(load_from(&store, Path::new("/tmp/rmaild.sock")).is_none());
}

#[test]
fn clearing_removes_a_cached_session() {
    let store = MemoryStore::default();
    let socket = Path::new("/tmp/rmaild.sock");
    save_to(&store, socket, &session("tok", now_unix() + 3600)).unwrap();

    clear_from(&store, socket).unwrap();
    assert!(load_from(&store, socket).is_none());
}

#[test]
fn different_sockets_do_not_share_a_cached_session() {
    let store = MemoryStore::default();
    let a = Path::new("/tmp/a.sock");
    let b = Path::new("/tmp/b.sock");
    save_to(&store, a, &session("tok-a", now_unix() + 3600)).unwrap();

    assert_eq!(load_from(&store, a).unwrap().token, "tok-a");
    assert!(load_from(&store, b).is_none());
}
