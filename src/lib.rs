#![feature(async_await)]
#![deny(missing_docs)]
//! # Futures-aware cache abstraction
//!
//! Provides a cache for asynchronous operations that persist data on the filesystem using RocksDB.
//!
//! The async cache works by accepting a future, but will cancel the accepted future in case the
//! answer is already in the cache.
//!
//! It requires unique cache keys that are `serde` serializable. To distinguish across different
//! sub-components of the cache, they can be namespaces using [`namespaced`].
//!
//! [`namespaced`]: Cache::namespaced

use chrono::{DateTime, Duration, Utc};
use futures::channel::oneshot;
use hashbrown::HashMap;
use hex::ToHex as _;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_cbor as cbor;
use serde_json as json;
use std::{collections::VecDeque, error, fmt, future::Future, sync::Arc};

/// Error type for the cache.
#[derive(Debug)]
pub enum Error {
    /// An underlying CBOR error.
    Cbor(cbor::error::Error),
    /// An underlying JSON error.
    Json(json::error::Error),
    /// An underlying RocksDB error.
    Rocksdb(rocksdb::Error),
    /// The underlying future failed (with an unspecified error).
    Failed,
}

impl fmt::Display for Error {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Cbor(e) => write!(fmt, "CBOR error: {}", e),
            Error::Json(e) => write!(fmt, "JSON error: {}", e),
            Error::Rocksdb(e) => write!(fmt, "RocksDB error: {}", e),
            Error::Failed => write!(fmt, "Operation failed"),
        }
    }
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Error::Cbor(e) => Some(e),
            Error::Json(e) => Some(e),
            Error::Rocksdb(e) => Some(e),
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

impl From<rocksdb::Error> for Error {
    fn from(error: rocksdb::Error) -> Self {
        Error::Rocksdb(error)
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

struct Inner {
    /// The serialized namespace this cache belongs to.
    ns: Option<cbor::Value>,
    /// Underlying storage.
    db: Arc<rocksdb::DB>,
    /// Things to wake up.
    wakers: Mutex<HashMap<Vec<u8>, VecDeque<oneshot::Sender<bool>>>>,
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
    pub fn load(db: Arc<rocksdb::DB>) -> Result<Cache, Error> {
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
    pub fn delete_with_ns<N, K>(&self, ns: Option<&N>, key: K) -> Result<(), Error>
    where
        N: Serialize,
        K: Serialize,
    {
        let ns = match ns {
            Some(ns) => Some(cbor::value::to_value(ns)?),
            None => None,
        };

        let key = self.key_with_ns(ns.as_ref(), key)?;
        self.inner.db.delete(&key)?;
        Ok(())
    }

    /// List all cache entries as JSON.
    pub fn list_json(&self) -> Result<Vec<JsonEntry>, Error> {
        let mut out = Vec::new();

        for (key, value) in self.inner.db.iterator(rocksdb::IteratorMode::Start) {
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

        for (key, value) in self.inner.db.iterator(rocksdb::IteratorMode::Start) {
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
                    self.inner.db.delete(key)?;
                    continue;
                }
            };

            if entry.is_expired(now) {
                self.inner.db.delete(key)?;
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
        // NB: Convert to value first to guarantee serialization order is consistent.
        let key = cbor::value::to_value(ns)?;

        Ok(Self {
            inner: Arc::new(Inner {
                ns: Some(key),
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
        self.inner.db.put(key, value)?;
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
            match self.inner_get(&key)? {
                State::Fresh(e) => return Ok(e.value),
                State::Expired(..) | State::Missing => (),
            };

            // Acquire a short-lived lock over over the wakers map.
            //
            // If an entry exist that means another `Wrap` instance is already computing
            // the answer.
            //
            // If it doesn't, we are the `Wrap` instance that will compute the answer.
            let rx = {
                let mut wakers = self.inner.wakers.lock();

                if let Some(queue) = wakers.get_mut(&key) {
                    let (tx, rx) = oneshot::channel();
                    queue.push_back(tx);
                    Some(rx)
                } else {
                    wakers.insert(key.clone(), VecDeque::with_capacity(16));
                    None
                }
            };

            if let Some(rx) = rx {
                // Ignore if sender is missing, just loop again.
                match rx.await {
                    Err(oneshot::Canceled) | Ok(true) => return Err(E::from(Error::Failed)),
                    Ok(false) => continue,
                }
            }

            break;
        }

        // Compute the answer by polling the underlying future and store it in the cache,
        // then acquire the wakers lock and dispatch to all pending futures.
        match future.await {
            Ok(output) => {
                self.inner_insert(&key, age, &output)?;
                self.cleanup_key(&key, false);
                Ok(output)
            }
            Err(e) => {
                self.cleanup_key(&key, true);
                Err(e)
            }
        }
    }

    /// Cleanup any dependents on the given key.
    fn cleanup_key(&self, key: &[u8], error: bool) {
        let mut wakers = match self.inner.wakers.lock().remove(key) {
            Some(wakers) => wakers,
            None => {
                log::warn!("no wakers registered for key: {}", KeyFormat(key));
                return;
            }
        };

        while let Some(waker) = wakers.pop_front() {
            let _ = waker.send(error);
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
    fn key_with_ns<T>(&self, ns: Option<&cbor::Value>, key: T) -> Result<Vec<u8>, Error>
    where
        T: Serialize,
    {
        // NB: needed to make sure key serialization is consistently ordered.
        // Internally serde_cbor uses ordered structured to store data.
        let key = cbor::value::to_value(key)?;
        let key = Key(ns, key);
        return Ok(cbor::to_vec(&key)?);

        #[derive(Serialize)]
        struct Key<'a>(Option<&'a cbor::Value>, cbor::Value);
    }
}

/// Helper formatter to convert cbor bytes to JSON or hex.
struct KeyFormat<'a>(&'a [u8]);

impl fmt::Display for KeyFormat<'_> {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match cbor::from_slice::<cbor::Value>(self.0) {
            Ok(value) => value,
            Err(_) => return self.0.write_hex(fmt),
        };

        let value = match json::to_string(&value) {
            Ok(value) => value,
            Err(_) => return self.0.write_hex(fmt),
        };

        value.fmt(fmt)
    }
}
