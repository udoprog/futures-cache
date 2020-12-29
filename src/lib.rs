#![deny(missing_docs)]
//! Futures-aware cache abstraction
//!
//! Provides a cache for asynchronous operations that persist data on the
//! filesystem using [sled]. The async cache works by accepting a future, but
//! will cancel the accepted future in case the answer is already in the cache.
//!
//! It requires unique cache keys that are [serde] serializable. To distinguish
//! across different sub-components of the cache, they can be namespaces using
//! [Cache::namespaced].
//!
//! [sled]: https://github.com/spacejam/sled
//!
//! ## State
//!
//! The state of the library is:
//! * API is limited to only `wrap`, which includes a timeout ([#1]).
//! * Requests are currently racing in the `wrap` method, so multiple unecessary
//!   requests might occur when they should //! instead be queueing up ([#2]).
//! * Entries only expire when the library is loaded ([#3]).
//! * Only storage backend is sled ([#4]).
//!
//! [#1]: https://github.com/udoprog/futures-cache/issues/1
//! [#2]: https://github.com/udoprog/futures-cache/issues/2
//! [#3]: https://github.com/udoprog/futures-cache/issues/3
//! [#4]: https://github.com/udoprog/futures-cache/issues/4
//!
//! ## Usage
//!
//! This library requires the user to add the following dependencies to use:
//!
//! ```toml
//! futures-cache = "0.3.0"
//! serde = {version = "1.0.97", features = ["derive"]}
//! ```
//!
//! ## Examples
//!
//! Simple example showcasing fetching information on a github repository.
//!
//! > This is also available as an example you can run with:
//! > ```
//! > cargo run --example github -- --user udoprog --repo futures-cache
//! > ```
//!
//! ```rust,no_run
//! use futures_cache::{Cache, Duration};
//! use serde::Serialize;
//!
//! type Error = Box<dyn std::error::Error>;
//!
//! #[derive(Debug, Serialize)]
//! enum GithubKey<'a> {
//!     Repo { user: &'a str, repo: &'a str },
//! }
//!
//! async fn github_repo(user: &str, repo: &str) -> Result<String, Error> {
//!     use reqwest::header;
//!     use reqwest::{Client, Url};
//!
//!     let client = Client::new();
//!
//!     let url = Url::parse(&format!("https://api.github.com/repos/{}/{}", user, repo))?;
//!
//!     let req = client
//!         .get(url)
//!         .header(header::USER_AGENT, "Reqwest/0.10.10")
//!         .build()?;
//!
//!     let body = client.execute(req).await?.text().await?;
//!     Ok(body)
//! }
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Error> {
//!     let db = sled::open("cache")?;
//!     let cache = Cache::load(db.open_tree("cache")?)?;
//!
//!     let user = "udoprog";
//!     let repo = "futures-cache";
//!
//!     let text = cache
//!         .wrap(
//!             GithubKey::Repo {
//!                 user: user,
//!                 repo: repo,
//!             },
//!             Duration::seconds(60),
//!             github_repo(user, repo),
//!         )
//!         .await?;
//!
//!     println!("{}", text);
//!     Ok(())
//! }
//! ```
//!
//! [serde]: https://docs.rs/serde
//! [Cache::namespaced]: https://docs.rs/futures-cache/0/futures_cache/struct.Cache.html#method.namespaced

use chrono::{DateTime, Utc};
use crossbeam::queue::SegQueue;
use futures_channel::oneshot;
use hashbrown::HashMap;
use hex::ToHex as _;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_cbor as cbor;
use serde_hashkey as hashkey;
use serde_json as json;
use std::error;
use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

pub use chrono::Duration;
pub use sled;

/// Error type for the cache.
#[derive(Debug)]
pub enum Error {
    /// An underlying CBOR error.
    Cbor(cbor::error::Error),
    /// An underlying HashKey error.
    HashKey(hashkey::Error),
    /// An underlying JSON error.
    Json(json::error::Error),
    /// An underlying Sled error.
    Sled(sled::Error),
    /// The underlying future failed (with an unspecified error).
    Failed,
}

impl fmt::Display for Error {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Cbor(e) => write!(fmt, "CBOR error: {}", e),
            Error::HashKey(e) => write!(fmt, "HashKey error: {}", e),
            Error::Json(e) => write!(fmt, "JSON error: {}", e),
            Error::Sled(e) => write!(fmt, "Database error: {}", e),
            Error::Failed => write!(fmt, "Operation failed"),
        }
    }
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Error::Cbor(e) => Some(e),
            Error::HashKey(e) => Some(e),
            Error::Json(e) => Some(e),
            Error::Sled(e) => Some(e),
            _ => None,
        }
    }
}

impl From<json::error::Error> for Error {
    fn from(error: json::error::Error) -> Self {
        Error::Json(error)
    }
}

impl From<cbor::error::Error> for Error {
    fn from(error: cbor::error::Error) -> Self {
        Error::Cbor(error)
    }
}

impl From<hashkey::Error> for Error {
    fn from(error: hashkey::Error) -> Self {
        Error::HashKey(error)
    }
}

impl From<sled::Error> for Error {
    fn from(error: sled::Error) -> Self {
        Error::Sled(error)
    }
}

/// Represents the state of an entry.
pub enum State<T> {
    /// Entry is fresh and can be used.
    Fresh(StoredEntry<T>),
    /// Entry exists, but is expired.
    /// Cache is referenced so that the value can be removed if needed.
    Expired(StoredEntry<T>),
    /// No entry.
    Missing,
}

impl<T> State<T> {
    /// Get as an option, regardless if it's expired or not.
    pub fn get(self) -> Option<T> {
        match self {
            State::Fresh(e) | State::Expired(e) => Some(e.value),
            State::Missing => None,
        }
    }
}

/// Entry which have had its type erased into a JSON representation for convenience.
///
/// This is necessary in case you want to list all the entries in the database unless you want to deal with raw bytes.
#[derive(Debug, Serialize, Deserialize)]
pub struct JsonEntry {
    /// The key of the entry.
    pub key: serde_json::Value,
    /// The stored entry.
    #[serde(flatten)]
    pub stored: StoredEntry<serde_json::Value>,
}

/// A complete stored entry with a type.
#[derive(Debug, Serialize, Deserialize)]
pub struct StoredEntry<T> {
    expires_at: DateTime<Utc>,
    value: T,
}

/// A reference to a complete stored entry with a type.
///
/// This is used for serialization to avoid taking ownership of the value to serialize.
#[derive(Debug, Serialize)]
pub struct StoredEntryRef<'a, T> {
    expires_at: DateTime<Utc>,
    value: &'a T,
}

impl<T> StoredEntry<T> {
    /// Test if entry is expired.
    fn is_expired(&self, now: DateTime<Utc>) -> bool {
        self.expires_at < now
    }
}

/// Used to only deserialize part of the stored entry.
#[derive(Debug, Serialize, Deserialize)]
struct PartialStoredEntry {
    expires_at: DateTime<Utc>,
}

impl PartialStoredEntry {
    /// Test if entry is expired.
    fn is_expired(&self, now: DateTime<Utc>) -> bool {
        self.expires_at < now
    }

    /// Convert into a stored entry.
    fn into_stored_entry(self) -> StoredEntry<()> {
        StoredEntry {
            expires_at: self.expires_at,
            value: (),
        }
    }
}

#[derive(Default)]
struct Waker {
    /// Number of things waiting for a response.
    pending: AtomicUsize,
    /// Channels to use for notifying dependents.
    channels: SegQueue<oneshot::Sender<bool>>,
}

impl Waker {
    /// Spin on performing cleanup, receiving channels to notify until we are in a stable state
    /// where everything has been reset.
    fn cleanup(&self, error: bool) {
        let mut previous = self.pending.load(Ordering::Acquire);

        loop {
            while previous > 1 {
                let mut received = 0usize;

                while let Some(waker) = self.channels.pop() {
                    received += 1;
                    let _ = waker.send(error);
                }

                // Subtract the number of notifications sent here. Setting this inside the wrap
                // function would deadlock on singlethreaded executors since they can't make
                // progress at the same time as this procedure.
                previous = self.pending.fetch_sub(received, Ordering::AcqRel);
            }

            previous = self.pending.compare_and_swap(1, 0, Ordering::AcqRel);

            if previous == 1 {
                break;
            }
        }
    }
}

struct Inner {
    /// The serialized namespace this cache belongs to.
    ns: Option<hashkey::Key>,
    /// Underlying storage.
    db: sled::Tree,
    /// Things to wake up.
    /// TODO: clean up wakers that have been idle for a long time in future cleanup loop.
    wakers: RwLock<HashMap<Vec<u8>, Arc<Waker>>>,
}

/// Primary cache abstraction.
///
/// Can be cheaply cloned and namespaced.
#[derive(Clone)]
pub struct Cache {
    inner: Arc<Inner>,
}

impl Cache {
    /// Load the cache from the database.
    pub fn load(db: sled::Tree) -> Result<Cache, Error> {
        let cache = Cache {
            inner: Arc::new(Inner {
                ns: None,
                db,
                wakers: Default::default(),
            }),
        };
        cache.cleanup()?;
        Ok(cache)
    }

    /// Delete the given key from the specified namespace.
    pub fn delete_with_ns<N, K>(&self, ns: Option<&N>, key: &K) -> Result<(), Error>
    where
        N: Serialize,
        K: Serialize,
    {
        let ns = match ns {
            Some(ns) => Some(hashkey::to_key(ns)?.normalize()),
            None => None,
        };

        let key = self.key_with_ns(ns.as_ref(), key)?;
        self.inner.db.remove(&key)?;
        Ok(())
    }

    /// List all cache entries as JSON.
    pub fn list_json(&self) -> Result<Vec<JsonEntry>, Error> {
        let mut out = Vec::new();

        for result in self.inner.db.range::<&[u8], _>(..) {
            let (key, value) = result?;

            let key: json::Value = match cbor::from_slice(&*key) {
                Ok(key) => key,
                // key is malformed.
                Err(_) => continue,
            };

            let stored = match cbor::from_slice(&*value) {
                Ok(storage) => storage,
                // something weird stored in there.
                Err(_) => continue,
            };

            out.push(JsonEntry { key, stored });
        }

        Ok(out)
    }

    /// Clean up stale entries.
    ///
    /// This could be called periodically if you want to reclaim space.
    fn cleanup(&self) -> Result<(), Error> {
        let now = Utc::now();

        for result in self.inner.db.range::<&[u8], _>(..) {
            let (key, value) = result?;

            let entry: PartialStoredEntry = match cbor::from_slice(&*value) {
                Ok(entry) => entry,
                Err(e) => {
                    if log::log_enabled!(log::Level::Trace) {
                        log::warn!(
                            "{}: failed to load: {}: {}",
                            KeyFormat(&*key),
                            e,
                            KeyFormat(&*value)
                        );
                    } else {
                        log::warn!("{}: failed to load: {}", KeyFormat(&*key), e);
                    }

                    // delete key since it's invalid.
                    self.inner.db.remove(key)?;
                    continue;
                }
            };

            if entry.is_expired(now) {
                self.inner.db.remove(key)?;
            }
        }

        Ok(())
    }

    /// Create a namespaced cache.
    ///
    /// The namespace must be unique to avoid conflicts.
    ///
    /// Each call to this functions will return its own queue for resolving futures.
    pub fn namespaced<N>(&self, ns: &N) -> Result<Self, Error>
    where
        N: Serialize,
    {
        Ok(Self {
            inner: Arc::new(Inner {
                ns: Some(hashkey::to_key(ns)?.normalize()),
                db: self.inner.db.clone(),
                wakers: Default::default(),
            }),
        })
    }

    /// Insert a value into the cache.
    pub fn insert<K, T>(&self, key: K, age: Duration, value: &T) -> Result<(), Error>
    where
        K: Serialize,
        T: Serialize,
    {
        let key = self.key(&key)?;
        self.inner_insert(&key, age, value)
    }

    /// Insert a value into the cache.
    #[inline(always)]
    fn inner_insert<T>(&self, key: &Vec<u8>, age: Duration, value: &T) -> Result<(), Error>
    where
        T: Serialize,
    {
        let expires_at = Utc::now() + age;

        let value = match cbor::to_vec(&StoredEntryRef { expires_at, value }) {
            Ok(value) => value,
            Err(e) => {
                log::trace!("store:{} *errored*", KeyFormat(key));
                return Err(e.into());
            }
        };

        log::trace!("store:{}", KeyFormat(key));
        self.inner.db.insert(key, value)?;
        Ok(())
    }

    /// Test an entry from the cache.
    pub fn test<K>(&self, key: K) -> Result<State<()>, Error>
    where
        K: Serialize,
    {
        let key = self.key(&key)?;
        self.inner_test(&key)
    }

    /// Load an entry from the cache.
    #[inline(always)]
    fn inner_test(&self, key: &[u8]) -> Result<State<()>, Error> {
        let value = match self.inner.db.get(&key)? {
            Some(value) => value,
            None => {
                log::trace!("test:{} -> null (missing)", KeyFormat(key));
                return Ok(State::Missing);
            }
        };

        let stored: PartialStoredEntry = match cbor::from_slice(&value) {
            Ok(value) => value,
            Err(e) => {
                if log::log_enabled!(log::Level::Trace) {
                    log::warn!(
                        "{}: failed to deserialize: {}: {}",
                        KeyFormat(key),
                        e,
                        KeyFormat(&value)
                    );
                } else {
                    log::warn!("{}: failed to deserialize: {}", KeyFormat(key), e);
                }

                log::trace!("test:{} -> null (deserialize error)", KeyFormat(key));
                return Ok(State::Missing);
            }
        };

        if stored.is_expired(Utc::now()) {
            log::trace!("test:{} -> null (expired)", KeyFormat(key));
            return Ok(State::Expired(stored.into_stored_entry()));
        }

        log::trace!("test:{} -> *value*", KeyFormat(key));
        Ok(State::Fresh(stored.into_stored_entry()))
    }

    /// Load an entry from the cache.
    pub fn get<K, T>(&self, key: K) -> Result<State<T>, Error>
    where
        K: Serialize,
        T: serde::de::DeserializeOwned,
    {
        let key = self.key(&key)?;
        self.inner_get(&key)
    }

    /// Load an entry from the cache.
    #[inline(always)]
    fn inner_get<T>(&self, key: &[u8]) -> Result<State<T>, Error>
    where
        T: serde::de::DeserializeOwned,
    {
        let value = match self.inner.db.get(key)? {
            Some(value) => value,
            None => {
                log::trace!("load:{} -> null (missing)", KeyFormat(key));
                return Ok(State::Missing);
            }
        };

        let stored: StoredEntry<T> = match cbor::from_slice(&value) {
            Ok(value) => value,
            Err(e) => {
                if log::log_enabled!(log::Level::Trace) {
                    log::warn!(
                        "{}: failed to deserialize: {}: {}",
                        KeyFormat(key),
                        e,
                        KeyFormat(&value)
                    );
                } else {
                    log::warn!("{}: failed to deserialize: {}", KeyFormat(key), e);
                }

                log::trace!("load:{} -> null (deserialize error)", KeyFormat(key));
                return Ok(State::Missing);
            }
        };

        if stored.is_expired(Utc::now()) {
            log::trace!("load:{} -> null (expired)", KeyFormat(key));
            return Ok(State::Expired(stored));
        }

        log::trace!("load:{} -> *value*", KeyFormat(key));
        Ok(State::Fresh(stored))
    }

    /// Get the waker associated with the given key.
    fn waker(&self, key: &[u8]) -> Arc<Waker> {
        let wakers = self.inner.wakers.read();

        match wakers.get(key) {
            Some(waker) => return waker.clone(),
            None => drop(wakers),
        }

        self.inner
            .wakers
            .write()
            .entry(key.to_vec())
            .or_default()
            .clone()
    }

    /// Wrap the result of the given future to load and store from cache.
    pub async fn wrap<'a, K, F, T, E>(&'a self, key: K, age: Duration, future: F) -> Result<T, E>
    where
        K: Serialize,
        F: Future<Output = Result<T, E>>,
        T: Serialize + serde::de::DeserializeOwned,
        E: From<Error>,
    {
        let key = self.key(&key)?;

        loop {
            // There a slight race here. The answer might _just_ have been provided when we perform
            // this check.
            //
            // If that happens, worst case we will end up re-computing the answer again.
            if let State::Fresh(e) = self.inner_get(&key)? {
                return Ok(e.value);
            }

            let waker = self.waker(&key);

            // only pending == 0 will be driving the future for a response.
            if waker.pending.fetch_add(1, Ordering::AcqRel) > 0 {
                let (tx, rx) = oneshot::channel();
                waker.channels.push(tx);

                let result = rx.await;

                // Ignore if sender is cancelled, just loop again.
                match result {
                    Ok(true) => return Err(E::from(Error::Failed)),
                    Err(oneshot::Canceled) | Ok(false) => continue,
                }
            }

            // Check key again in case we got really unlucky and had two call do an interleaving
            // pass for the previous check:
            //
            // T1 just went passed the first inner_get test above.
            // T2 just finished the Waker::cleanup procedure and reduces pending to 0.
            // T1 notices that it is the first pending thread (pending == 0) and ends up here.
            if let State::Fresh(e) = self.inner_get(&key)? {
                waker.cleanup(false);
                return Ok(e.value);
            }

            // Guard in case it is cancelled.
            let result = Guard::new(|| waker.cleanup(false)).wrap(future).await;

            // Compute the answer by polling the underlying future and store it in the cache,
            // then acquire the wakers lock and dispatch to all pending futures.
            match result {
                Ok(output) => {
                    self.inner_insert(&key, age, &output)?;
                    waker.cleanup(false);
                    return Ok(output);
                }
                Err(e) => {
                    waker.cleanup(true);
                    return Err(e);
                }
            }
        }

        /// Create a stack guard that will run unless it is forgotten.
        struct Guard<F>
        where
            F: FnMut(),
        {
            f: F,
        }

        impl<F> Guard<F>
        where
            F: FnMut(),
        {
            /// Construct a new finalizer.
            pub fn new(f: F) -> Self {
                Self { f }
            }

            /// Wrap the given future with this cancellation guard.
            pub async fn wrap<O>(self, future: O) -> O::Output
            where
                O: Future,
            {
                let result = future.await;
                std::mem::forget(self);
                result
            }
        }

        impl<F> Drop for Guard<F>
        where
            F: FnMut(),
        {
            fn drop(&mut self) {
                (self.f)();
            }
        }
    }

    /// Helper to serialize the key with the default namespace.
    fn key<T>(&self, key: &T) -> Result<Vec<u8>, Error>
    where
        T: Serialize,
    {
        self.key_with_ns(self.inner.ns.as_ref(), key)
    }

    /// Helper to serialize the key with a specific namespace.
    fn key_with_ns<T>(&self, ns: Option<&hashkey::Key>, key: &T) -> Result<Vec<u8>, Error>
    where
        T: Serialize,
    {
        let key = hashkey::to_key(key)?.normalize();
        let key = Key(ns, key);
        return Ok(cbor::to_vec(&key)?);

        #[derive(Serialize)]
        struct Key<'a>(Option<&'a hashkey::Key>, hashkey::Key);
    }
}

/// Helper formatter to convert cbor bytes to JSON or hex.
struct KeyFormat<'a>(&'a [u8]);

impl fmt::Display for KeyFormat<'_> {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match cbor::from_slice::<cbor::Value>(self.0) {
            Ok(value) => value,
            Err(_) => return self.0.encode_hex::<String>().fmt(fmt),
        };

        let value = match json::to_string(&value) {
            Ok(value) => value,
            Err(_) => return self.0.encode_hex::<String>().fmt(fmt),
        };

        value.fmt(fmt)
    }
}

#[cfg(test)]
mod tests {
    use super::{Cache, Duration, Error};
    use std::{error, fs, sync::Arc, thread};
    use tempdir::TempDir;

    fn db(name: &str) -> Result<sled::Tree, Box<dyn error::Error>> {
        let path = TempDir::new(name)?;
        let path = path.path();

        if !path.is_dir() {
            fs::create_dir_all(path)?;
        }

        let db = sled::open(path)?;
        Ok(db.open_tree("test")?)
    }

    #[test]
    fn test_cached() -> Result<(), Box<dyn error::Error>> {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let db = db("test_cached")?;
        let cache = Cache::load(db)?;

        let count = Arc::new(AtomicUsize::default());

        let c = count.clone();

        let op1 = cache.wrap("a", Duration::hours(12), async move {
            let _ = c.fetch_add(1, Ordering::SeqCst);
            Ok::<_, Error>(String::from("foo"))
        });

        let c = count.clone();

        let op2 = cache.wrap("a", Duration::hours(12), async move {
            let _ = c.fetch_add(1, Ordering::SeqCst);
            Ok::<_, Error>(String::from("foo"))
        });

        ::futures::executor::block_on(async move {
            let (a, b) = ::futures::future::join(op1, op2).await;
            assert_eq!("foo", a.expect("ok result"));
            assert_eq!("foo", b.expect("ok result"));
            assert_eq!(1, count.load(Ordering::SeqCst));
        });

        Ok(())
    }

    #[test]
    fn test_contended() -> Result<(), Box<dyn error::Error>> {
        use crossbeam::queue::SegQueue;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        const THREAD_COUNT: usize = 1_000;

        let db = db("test_contended")?;
        let cache = Cache::load(db)?;

        let started = Arc::new(AtomicBool::new(false));
        let count = Arc::new(AtomicUsize::default());
        let results = Arc::new(SegQueue::new());
        let mut threads = Vec::with_capacity(THREAD_COUNT);

        for _ in 0..THREAD_COUNT {
            let started = started.clone();
            let cache = cache.clone();
            let results = results.clone();
            let count = count.clone();

            let t = thread::spawn(move || {
                let op = cache.wrap("a", Duration::hours(12), async move {
                    let _ = count.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, Error>(String::from("foo"))
                });

                while !started.load(Ordering::Acquire) {}

                ::futures::executor::block_on(async move {
                    results.push(op.await);
                });
            });

            threads.push(t);
        }

        started.store(true, Ordering::Release);

        for t in threads {
            t.join().expect("thread to join");
        }

        assert_eq!(1, count.load(Ordering::SeqCst));
        Ok(())
    }

    #[test]
    fn test_guards() -> Result<(), Box<dyn error::Error>> {
        use self::futures::PollOnce;
        use ::futures::channel::oneshot;
        use std::sync::atomic::Ordering;

        let db = db("test_guards")?;
        let cache = Cache::load(db)?;

        ::futures::executor::block_on(async move {
            let (op1_tx, op1_rx) = oneshot::channel::<()>();

            let op1 = cache.wrap("a", Duration::hours(12), async move {
                let _ = op1_rx.await;
                Ok::<_, Error>(String::from("foo"))
            });

            pin_utils::pin_mut!(op1);

            let (op2_tx, op2_rx) = oneshot::channel::<()>();

            let op2 = cache.wrap("a", Duration::hours(12), async move {
                let _ = op2_rx.await;
                Ok::<_, Error>(String::from("foo"))
            });

            pin_utils::pin_mut!(op2);

            assert!(PollOnce::new(&mut op1).await.is_none());

            let k = cache.key(&"a")?;
            let waker = cache.inner.wakers.read().get(&k).cloned();
            assert!(waker.is_some());
            let waker = waker.expect("waker to be registered");

            assert_eq!(1, waker.pending.load(Ordering::SeqCst));
            assert!(PollOnce::new(&mut op2).await.is_none());
            assert_eq!(2, waker.pending.load(Ordering::SeqCst));

            op1_tx.send(()).expect("send to op1");
            op2_tx.send(()).expect("send to op2");

            assert!(PollOnce::new(&mut op1).await.is_some());
            assert_eq!(0, waker.pending.load(Ordering::SeqCst));
            assert!(PollOnce::new(&mut op2).await.is_some());

            Ok(())
        })
    }

    mod futures {
        use std::{
            future::Future,
            pin::Pin,
            task::{Context, Poll},
        };

        pub struct PollOnce<F> {
            future: F,
        }

        impl<F> PollOnce<F> {
            /// Wrap a new future to be polled once.
            pub fn new(future: F) -> Self {
                Self { future }
            }
        }

        impl<F> PollOnce<F> {
            pin_utils::unsafe_pinned!(future: F);
        }

        impl<F> Future for PollOnce<F>
        where
            F: Future,
        {
            type Output = Option<F::Output>;

            fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
                match self.future().poll(cx) {
                    Poll::Ready(output) => Poll::Ready(Some(output)),
                    Poll::Pending => Poll::Ready(None),
                }
            }
        }
    }
}
