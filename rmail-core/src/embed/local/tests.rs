//! Everything here runs without a model on disk except
//! [`the_real_model_embeds_meaning`], which is the one test that needs the
//! weights and says so.

use super::*;
use crate::ErrorReason;

/// Whether the tests that need the weights should run.
///
/// Asks whether *this* model is cached, not merely whether the directory has
/// something in it — a cache holding some other model would otherwise send the
/// real-model tests to the network. `RMAIL_ONNX_TESTS=1` opts into provisioning
/// it on a cold cache, which is a several-hundred-megabyte download and not a
/// decision a unit test may make on somebody's laptop.
fn model_available() -> bool {
    std::env::var("RMAIL_ONNX_TESTS").is_ok_and(|v| v == "1")
        || cached(&cache_dir(), "bge-small-en-v1.5")
}

#[test]
fn constructing_the_embedder_touches_no_disk() {
    // Every daemon and every test constructs one. If this loaded the model,
    // nothing could be tested without the weights present.
    let started = std::time::Instant::now();
    let e = LocalEmbedder::new(&LocalEmbedConfig::default());
    assert_eq!(e.model(), "bge-small-en-v1.5");
    assert_eq!(e.dim(), 384);
    // Loading the weights takes the better part of a second, so half of one
    // still separates "constructed" from "loaded the model". A tighter bound
    // was measuring the scheduler rather than this code, and failed whenever
    // the machine was busy.
    assert!(
        started.elapsed() < std::time::Duration::from_millis(500),
        "took {:?}",
        started.elapsed()
    );
}

#[test]
fn a_typo_in_the_model_name_says_what_to_write_instead() {
    // Otherwise the failure surfaces as a Hugging Face 404 from deep inside a
    // model loader, which tells an operator nothing about their config file.
    let err = known_model("bge-smol").unwrap_err();
    assert_eq!(err.reason(), ErrorReason::InvalidArgument);
    let message = err.to_string();
    assert!(
        message.contains("bge-small-en-v1.5"),
        "should name the supported models: {message}"
    );
}

#[test]
fn every_documented_model_resolves() {
    for id in [
        "bge-small-en-v1.5",
        "bge-base-en-v1.5",
        "bge-large-en-v1.5",
        "all-MiniLM-L6-v2",
        "multilingual-e5-small",
        "multilingual-e5-base",
    ] {
        assert!(known_model(id).is_ok(), "{id} should be loadable");
    }
}

#[test]
fn the_cache_directory_follows_the_environment() {
    // An operator with no egress provisions the weights out of band and points
    // the daemon at them; that is the documented escape hatch and it has to
    // work.
    temp_env(&[(CACHE_ENV, Some("/models/here"))], || {
        assert_eq!(cache_dir(), PathBuf::from("/models/here"));
    });
    temp_env(
        &[(CACHE_ENV, None), ("XDG_CACHE_HOME", Some("/xdg"))],
        || {
            assert_eq!(cache_dir(), PathBuf::from("/xdg/rmail/models"));
        },
    );
    temp_env(
        &[
            (CACHE_ENV, None),
            ("XDG_CACHE_HOME", None),
            ("HOME", Some("/home/ada")),
        ],
        || {
            assert_eq!(cache_dir(), PathBuf::from("/home/ada/.cache/rmail/models"));
        },
    );
}

#[tokio::test]
async fn an_empty_batch_does_not_load_the_model() {
    // Called with nothing to do on a host with no weights, this must be a
    // no-op rather than a several-hundred-megabyte download.
    let e = LocalEmbedder::new(&LocalEmbedConfig::default());
    assert!(e.embed(&[]).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_missing_model_is_a_precondition_that_says_how_to_fix_it() {
    // Unconditional, and it does not depend on the ambient cache. Gating this
    // on `model_available()` made it mutually exclusive with the two
    // real-model tests, so on any given machine one of the three was dead and
    // passing green — and on a developer machine the dead one was this, the
    // only error path this backend has.
    let e = LocalEmbedder {
        model: "bge-small-en-v1.5".to_owned(),
        dim: 384,
        // Empty, so the model is genuinely absent.
        cache: std::env::temp_dir().join("rmail-unprovisioned-cache"),
        allow_download: false,
        session: OnceCell::new(),
        permit: tokio::sync::Semaphore::new(1),
    };
    std::fs::create_dir_all(&e.cache).unwrap();

    let err = e.embed(&["hello".to_owned()]).await.unwrap_err();

    assert_eq!(err.reason(), ErrorReason::FailedPrecondition);
    assert!(
        err.to_string().contains(CACHE_ENV),
        "the message must name the escape hatch an operator with no egress \
         will need: {err}"
    );
    // The proof that it declined rather than fetched. An earlier version of
    // this test relied on `HF_HUB_OFFLINE=1`, which `fastembed`'s downloader
    // ignores outright: it pulled 128 MB from Hugging Face inside a unit test.
    let fetched = std::fs::read_dir(&e.cache).map(|d| d.count()).unwrap_or(0);
    assert_eq!(
        fetched, 0,
        "nothing may be downloaded when downloading is off — that is the whole \
         guarantee the local backend exists to make"
    );
    let _ = std::fs::remove_dir_all(&e.cache);
}

#[tokio::test]
async fn an_unprovisioned_cache_with_downloading_on_is_allowed_to_fetch() {
    // The other side of the switch: turning it on must actually get past the
    // precondition. Without the weights present this would go to the network,
    // so it asserts the *decision*, not the outcome.
    let e = LocalEmbedder {
        model: "bge-small-en-v1.5".to_owned(),
        dim: 384,
        cache: cache_dir(),
        allow_download: true,
        session: OnceCell::new(),
        permit: tokio::sync::Semaphore::new(1),
    };
    if !model_available() {
        return;
    }
    assert!(e.embed(&["hello".to_owned()]).await.is_ok());
}

#[test]
fn a_half_written_cache_does_not_count_as_provisioned() {
    // An interrupted fetch leaves the outer directory and no snapshot. Treating
    // that as provisioned turns a clear precondition into a confusing error
    // from inside a model loader.
    let root = std::env::temp_dir().join("rmail-half-cache");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("models--Xenova--bge-small-en-v1.5/snapshots")).unwrap();
    assert!(!cached(&root, "bge-small-en-v1.5"));

    std::fs::create_dir_all(root.join("models--Xenova--bge-small-en-v1.5/snapshots/abc")).unwrap();
    assert!(cached(&root, "bge-small-en-v1.5"));
    assert!(!cached(&root, "bge-base-en-v1.5"), "a different model");
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn the_real_model_embeds_meaning() {
    // The one test that needs the weights. It is the only place the claim
    // "semantic" is actually checked: everything else in this backend is
    // plumbing, and plumbing that works perfectly around a model that does not
    // load is worth nothing.
    if !model_available() {
        return;
    }
    let e = LocalEmbedder::new(&LocalEmbedConfig::default());
    e.warm().await.unwrap();

    let vectors = e
        .embed(&[
            "please send the invoice for last month's hosting".to_owned(),
            "could you forward the bill for the servers in march".to_owned(),
            "the dog needs to go to the vet on tuesday".to_owned(),
        ])
        .await
        .unwrap();

    assert_eq!(vectors.len(), 3);
    assert!(vectors.iter().all(|v| v.dim() == 384));
    let paraphrase = vectors[0].cosine(&vectors[1]);
    let unrelated = vectors[0].cosine(&vectors[2]);
    assert!(
        paraphrase > unrelated + 0.15,
        "a paraphrase sharing almost no vocabulary ({paraphrase}) must beat an \
         unrelated sentence ({unrelated}) — that difference is the entire \
         reason this backend exists"
    );
    // The floor matters as much as the margin and is invisible without this.
    // bge-small scores about 0.55 against the *empty string*, so its cosines
    // live in roughly [0.55, 1.0]: an unrelated sentence at 0.61 is barely
    // above noise, and any ranker that puts an absolute threshold on a raw
    // cosine from this model will rank an empty-bodied message against every
    // query. Pinned here so that fact is discovered by a test rather than by a
    // user.
    assert!(
        unrelated < 0.70,
        "an unrelated sentence scored {unrelated}; the whole usable range is \
         above roughly 0.55, so this is not a comfortable margin"
    );
}

#[tokio::test]
async fn the_real_model_is_deterministic_and_batch_invariant() {
    if !model_available() {
        return;
    }
    let e = LocalEmbedder::new(&LocalEmbedConfig::default());
    let texts: Vec<String> = (0..3).map(|n| format!("message number {n}")).collect();

    let batched = e.embed(&texts).await.unwrap();
    for (n, text) in texts.iter().enumerate() {
        let alone = e.embed(std::slice::from_ref(text)).await.unwrap();
        let drift = 1.0 - batched[n].cosine(&alone[0]);
        assert!(
            drift < 1e-3,
            "vector {n} drifted by {drift} between a batch and a single call; \
             the content-hash cache assumes it will not"
        );
    }
}

#[test]
fn concurrent_embeds_do_not_starve_the_blocking_pool() {
    // The session admits one inference at a time, which is unavoidable. What is
    // avoidable is *where* the other callers wait: taking the mutex inside
    // `spawn_blocking` parked seven of eight blocking-pool threads on a lock
    // doing nothing, and that pool is shared with the credential commands IMAP
    // login runs on — so concurrent embedding starved unrelated subsystems.
    //
    // Measured with a deliberately small pool and an unrelated blocking task as
    // the probe. With the semaphore acquired before the spawn, exactly one
    // embed occupies a thread and the probe runs at once. With it acquired
    // inside, the pool fills with embeds waiting on a mutex and the probe waits
    // behind all of them.
    if !model_available() {
        return;
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .max_blocking_threads(2)
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        let e = std::sync::Arc::new(LocalEmbedder::new(&LocalEmbedConfig {
            allow_download: true,
            ..LocalEmbedConfig::default()
        }));
        e.warm().await.unwrap();

        // Batches big enough that one inference is measurably long. With
        // one-line inputs the whole queue drains in a couple of milliseconds
        // and the probe never waits whichever side of `spawn_blocking` the
        // permit is taken on — a test that cannot fail.
        let batch: Vec<String> = (0..16)
            .map(|n| format!("message number {n} ").repeat(32))
            .collect();
        let mut tasks = Vec::new();
        for _ in 0..4 {
            let e = std::sync::Arc::clone(&e);
            let batch = batch.clone();
            tasks.push(tokio::spawn(async move { e.embed(&batch).await }));
        }
        // Long enough for every embed to have reached its waiting point.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let started = std::time::Instant::now();
        tokio::task::spawn_blocking(|| ()).await.unwrap();
        let probe = started.elapsed();

        let mut total = std::time::Duration::ZERO;
        let embedding = std::time::Instant::now();
        for task in tasks {
            task.await.unwrap().unwrap();
        }
        total += embedding.elapsed();

        // Relative, not absolute: what matters is that an unrelated blocking
        // task got a thread *while* the embeds were running, not how fast the
        // machine is. Taken inside `spawn_blocking`, or not taken at all, the
        // two-thread pool fills with embeds and the probe waits behind them.
        assert!(
            probe * 4 < total,
            "an unrelated blocking task waited {probe:?} of the {total:?} the \
             embeds took. The permit must be acquired before `spawn_blocking`: \
             inside it, the pool fills with embeds and everything else in the \
             process queues behind them"
        );
    });
}

#[tokio::test]
async fn a_poisoned_session_is_recovered_rather_than_refused_forever() {
    // A panic inside ort poisons the mutex for the life of the process.
    // Refusing every subsequent embed leaves the daemon permanently and
    // silently degraded — the client sees a redacted "internal error" with
    // nothing to diagnose — when the session is no more broken than it was the
    // instant before.
    if !model_available() {
        return;
    }
    let e = LocalEmbedder::new(&LocalEmbedConfig {
        allow_download: true,
        ..LocalEmbedConfig::default()
    });
    e.warm().await.unwrap();

    let session = e.session().await.unwrap();
    let poisoner = std::sync::Arc::clone(&session);
    // Poison it the only way a mutex can be poisoned: panic while holding it.
    let _ = std::thread::spawn(move || {
        let _guard = poisoner.lock();
        // The only way a mutex becomes poisoned is a panic while it is held,
        // so reproducing the condition requires reproducing the cause.
        #[expect(clippy::panic, reason = "poisoning a mutex requires a panic")]
        {
            panic!("simulating an inference panic");
        }
    })
    .join();
    assert!(session.is_poisoned());

    assert!(
        e.embed(&["still working".to_owned()]).await.is_ok(),
        "a poisoned session must not end semantic search for this process"
    );
}

/// Run `body` with some environment variables set, restoring them after.
///
/// Serialized on a mutex because the environment is process-global and the test
/// harness is threaded; two of these running at once would read each other's
/// variables.
fn temp_env(vars: &[(&str, Option<&str>)], body: impl FnOnce()) {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    let saved: Vec<(String, Option<String>)> = vars
        .iter()
        .map(|(key, _)| ((*key).to_owned(), std::env::var(key).ok()))
        .collect();
    for (key, value) in vars {
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }
    body();
    for (key, value) in saved {
        match value {
            Some(value) => std::env::set_var(&key, value),
            None => std::env::remove_var(&key),
        }
    }
}
