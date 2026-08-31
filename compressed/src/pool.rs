// Licensed under the MIT License.

//! Reuse of compression engine state across codecs.

#[cfg(any(feature = "deflate", feature = "gzip", feature = "zlib"))]
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
#[cfg(any(feature = "deflate", feature = "gzip", feature = "zlib"))]
use std::sync::Mutex;

#[cfg(any(feature = "deflate", feature = "gzip", feature = "zlib"))]
use crate::flate::Wrapper;

/// How many idle engines the pool keeps per distinct configuration, unless told otherwise.
const DEFAULT_CAPACITY: usize = 16;

/// Identifies engines that are interchangeable with one another.
///
/// An engine can only be reused for the configuration it was built with: resetting a compressor
/// preserves its container and its level, so a gzip level-9 engine cannot serve a zlib level-1
/// request.
#[cfg(any(feature = "deflate", feature = "gzip", feature = "zlib"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct EngineKey {
    pub(crate) wrapper: Wrapper,
    pub(crate) level: u8,
}

/// A shared, cloneable pool of reusable compression engine state.
///
/// Building a compressor is not free: constructing a gzip compressor costs about **6.9 µs**, which
/// is comparable to the ~11.2 µs it takes to compress a 10 KiB body. A service that builds a fresh
/// encoder per request therefore spends much of its compression budget on setup. Measured end to
/// end, per request:
///
/// | Body | Fresh engine | Pooled engine | Saved |
/// |---|---|---|---|
/// | 1 KiB | 11.8 µs | 6.0 µs | **~50%** |
/// | 10 KiB | 8.4 µs | 6.2 µs | **~26%** |
///
/// The saving is a roughly fixed ~6 µs per encoder, so it matters most for small messages and
/// fades into the noise for large ones — which is the shape of ordinary request and response
/// traffic, where most bodies are small.
///
/// Clone is cheap and every clone shares one pool, so a client holds a single pool and clones it
/// into each request:
///
/// ```
/// use bytesbuf::BytesView;
/// use bytesbuf::mem::GlobalPool;
/// use compressed::{Level, Pool, gzip};
///
/// #[derive(Clone)]
/// struct HttpClient {
///     codecs: Pool,
///     memory: GlobalPool,
/// }
///
/// impl HttpClient {
///     fn compress_body(&self, body: BytesView) -> compressed::Result<BytesView> {
///         let mut encoder = gzip::Encoder::builder()
///             .level(Level::DEFAULT)
///             .pool(self.codecs.clone())
///             .build(self.memory.clone());
///
///         encoder.push(body)?;
///         encoder.finish();
///
///         let mut parts = Vec::new();
///         while let Some(chunk) = encoder.pull()?.into_data() {
///             parts.push(chunk);
///         }
///         Ok(BytesView::from_views(parts))
///         // Dropping `encoder` returns its engine to the pool for the next request.
///     }
/// }
///
/// let client = HttpClient {
///     codecs: Pool::new(),
///     memory: GlobalPool::new(),
/// };
/// let body = BytesView::copied_from_slice(b"a request body", &client.memory);
///
/// // Recycling is invisible: the second request produces exactly the first request's bytes.
/// let first = client.compress_body(body.clone())?;
/// let second = client.compress_body(body)?;
/// assert_eq!(first.to_vec(), second.to_vec());
/// # Ok::<(), compressed::Error>(())
/// ```
///
/// # What is actually pooled
///
/// The pool is transparent: it recycles the engines that are worth recycling and silently builds
/// the rest, so calling code never has to know which is which. Today that means **compressors for
/// [`deflate`][crate::deflate], [`zlib`][crate::zlib] and [`gzip`][crate::gzip]**. Measured, the
/// engines it does not pool are not worth pooling:
///
/// | Engine | Construction | Reusable? |
/// |---|---|---|
/// | deflate-family compressor | 6.9 µs | yes — `reset` preserves container and level |
/// | deflate-family decompressor | 0.4 µs | not worth it, and a reset cannot restore gzip framing |
/// | brotli encoder | 0.2 µs | no reset exists upstream |
/// | brotli decoder | 0.2 µs | no reset exists upstream |
///
/// Because this is an implementation detail rather than a contract, more engines can start being
/// pooled without any change to calling code.
///
/// # Bounds
///
/// The pool keeps at most [`Pool::capacity`] idle engines per distinct configuration, so a burst of
/// concurrent requests cannot make it grow without limit. Engines beyond that are dropped when they
/// are returned.
#[derive(Clone)]
pub struct Pool {
    inner: Arc<Inner>,
}

struct Inner {
    #[cfg(any(feature = "deflate", feature = "gzip", feature = "zlib"))]
    compressors: Mutex<HashMap<EngineKey, Vec<flate2::Compress>>>,
    capacity: usize,
}

impl Pool {
    /// Creates a pool that keeps up to 16 idle engines per configuration.
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    /// Creates a pool that keeps up to `capacity` idle engines per configuration.
    ///
    /// Size this to the number of messages you expect to be encoding at once. A capacity of zero
    /// disables recycling, which is useful for measuring what the pool is buying you.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            inner: Arc::new(Inner {
                #[cfg(any(feature = "deflate", feature = "gzip", feature = "zlib"))]
                compressors: Mutex::new(HashMap::new()),
                capacity,
            }),
        }
    }

    /// The most idle engines this pool keeps per distinct configuration.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.inner.capacity
    }

    /// Takes an idle compressor for `key`, or reports that one must be built.
    ///
    /// The engine is reset before it is handed over, so a codec dropped part-way through a stream
    /// cannot leak its state into the next user.
    #[cfg(any(feature = "deflate", feature = "gzip", feature = "zlib"))]
    pub(crate) fn take_compressor(&self, key: EngineKey) -> Option<flate2::Compress> {
        // A poisoned pool is not worth propagating: recycling is an optimisation, so building a
        // fresh engine is always preferable to failing the caller's compression.
        let mut engine = self.inner.compressors.lock().ok()?.get_mut(&key).and_then(Vec::pop)?;

        engine.reset();
        Some(engine)
    }

    /// Returns a compressor for reuse, dropping it if the pool is already full.
    #[cfg(any(feature = "deflate", feature = "gzip", feature = "zlib"))]
    pub(crate) fn return_compressor(&self, key: EngineKey, engine: flate2::Compress) {
        if self.inner.capacity == 0 {
            return;
        }

        if let Ok(mut guard) = self.inner.compressors.lock() {
            let idle = guard.entry(key).or_default();
            if idle.len() < self.inner.capacity {
                idle.push(engine);
            }
        }
    }
}

impl Default for Pool {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Pool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pool")
            .field("capacity", &self.inner.capacity)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_capacity_is_applied() {
        assert_eq!(Pool::new().capacity(), DEFAULT_CAPACITY);
        assert_eq!(Pool::default().capacity(), DEFAULT_CAPACITY);
        assert_eq!(Pool::with_capacity(3).capacity(), 3);
    }

    #[test]
    fn clones_share_one_pool() {
        let pool = Pool::new();
        let clone = pool.clone();

        assert!(Arc::ptr_eq(&pool.inner, &clone.inner), "cloning must not fork the pool");
    }

    #[test]
    fn debug_reports_capacity() {
        assert!(format!("{:?}", Pool::with_capacity(4)).contains("capacity: 4"));
    }

    #[cfg(feature = "gzip")]
    mod pooling {
        use super::*;
        use crate::Level;

        fn key(level: u8) -> EngineKey {
            EngineKey {
                wrapper: Wrapper::Gzip,
                level,
            }
        }

        fn engine() -> flate2::Compress {
            Wrapper::Gzip.compressor(Level::DEFAULT)
        }

        /// Counts what the pool is holding, which the public API deliberately does not expose.
        fn idle(pool: &Pool, key: EngineKey) -> usize {
            pool.inner
                .compressors
                .lock()
                .expect("pool is not poisoned")
                .get(&key)
                .map_or(0, Vec::len)
        }

        #[test]
        fn an_engine_survives_a_round_trip_through_the_pool() {
            let pool = Pool::new();
            assert!(pool.take_compressor(key(6)).is_none(), "an empty pool has nothing to give");

            pool.return_compressor(key(6), engine());
            assert_eq!(idle(&pool, key(6)), 1);

            assert!(pool.take_compressor(key(6)).is_some(), "the returned engine should come back");
            assert_eq!(idle(&pool, key(6)), 0, "taking an engine removes it from the pool");
        }

        #[test]
        fn engines_are_not_shared_between_configurations() {
            let pool = Pool::new();
            pool.return_compressor(key(6), engine());

            assert!(
                pool.take_compressor(key(9)).is_none(),
                "a level-9 request must not receive a level-6 engine"
            );
        }

        #[test]
        fn capacity_bounds_what_is_retained() {
            let pool = Pool::with_capacity(2);
            for _ in 0..5 {
                pool.return_compressor(key(6), engine());
            }

            assert_eq!(idle(&pool, key(6)), 2, "only `capacity` engines are kept");
        }

        #[test]
        fn zero_capacity_disables_recycling() {
            let pool = Pool::with_capacity(0);
            pool.return_compressor(key(6), engine());

            assert_eq!(idle(&pool, key(6)), 0);
            assert!(pool.take_compressor(key(6)).is_none());
        }

        #[test]
        fn a_returned_engine_is_reset_before_reuse() {
            // An engine abandoned mid-stream must not leak its state into the next user.
            let mut dirty = engine();
            let mut scratch = [0_u8; 256];
            dirty
                .compress(b"half a stream", &mut scratch, flate2::FlushCompress::None)
                .expect("compress");
            assert!(dirty.total_in() > 0, "the engine should be dirty");

            let pool = Pool::new();
            pool.return_compressor(key(6), dirty);

            let clean = pool.take_compressor(key(6)).expect("the engine comes back");
            assert_eq!(clean.total_in(), 0, "checkout must reset the engine");
            assert_eq!(clean.total_out(), 0);
        }
    }
}
