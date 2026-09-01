// Licensed under the MIT License.

use std::fmt;

use bytesbuf::{BytesBuf, BytesView};

use crate::error::Result;
use crate::output::Output;

mod sealed {
    pub trait Compression {}

    #[cfg(feature = "brotli")]
    impl Compression for crate::brotli::Compressor {}
    #[cfg(feature = "brotli")]
    impl Compression for crate::brotli::Decompressor {}
    #[cfg(feature = "deflate")]
    impl Compression for crate::deflate::Compressor {}
    #[cfg(feature = "deflate")]
    impl Compression for crate::deflate::Decompressor {}
    #[cfg(feature = "gzip")]
    impl Compression for crate::gzip::Compressor {}
    #[cfg(feature = "gzip")]
    impl Compression for crate::gzip::Decompressor {}
    #[cfg(feature = "zlib")]
    impl Compression for crate::zlib::Compressor {}
    #[cfg(feature = "zlib")]
    impl Compression for crate::zlib::Decompressor {}
    #[cfg(feature = "zstd")]
    impl Compression for crate::zstd::Compressor {}
    #[cfg(feature = "zstd")]
    impl Compression for crate::zstd::Decompressor {}

    impl<D> Compression for Box<dyn super::Compression<Mode = D>> {}
}

/// Marks a [`Compression`] implementation that compresses its input.
///
/// This marker cannot be constructed outside this crate.
#[derive(Debug)]
#[non_exhaustive]
pub struct Compress;

/// Marks a [`Compression`] implementation that decompresses its input.
///
/// This marker cannot be constructed outside this crate.
#[derive(Debug)]
#[non_exhaustive]
pub struct Decompress;

/// A streaming compression or decompression operation.
///
/// Every format's compressor and decompressor implements this contract. The `Mode` associated type
/// records which operation an implementation performs without changing how callers drive it. This
/// allows shared processing code to accept any `Compression`, while APIs that require one direction
/// can use `Compression<Mode = Compress>` or `Compression<Mode = Decompress>`.
///
/// The trait is sealed so formats and methods can be added without breaking downstream code.
/// Every implementation is `Send + Sync`.
///
/// # Examples
///
/// ```
/// use bytesbuf::BytesView;
/// use bytesbuf::mem::{GlobalPool, MemoryShared};
/// use compressed::{Compress, Compression, Output, gzip};
///
/// fn compress(
///     mut compression: impl Compression<Mode = Compress>,
///     input: BytesView,
/// ) -> compressed::Result<BytesView> {
///     compression.process(input)
/// }
///
/// let memory = GlobalPool::new();
/// let compressed = compress(
///     gzip::Compressor::new(memory.clone()),
///     BytesView::copied_from_slice(b"format agnostic", &memory),
/// )?;
///
/// assert_eq!(compressed.range(0..2).to_vec(), vec![0x1f, 0x8b]);
/// # Ok::<(), compressed::Error>(())
/// ```
pub trait Compression: sealed::Compression + fmt::Debug + Send + Sync {
    /// Whether this implementation compresses or decompresses its input.
    type Mode;

    /// Supplies more input.
    ///
    /// # Errors
    ///
    /// Returns an error if input is still pending or the operation has been finished.
    fn push(&mut self, input: BytesView) -> Result<()>;

    /// Signals that no further input will be supplied.
    fn finish(&mut self);

    /// Produces the next output chunk.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying engine fails or the input is invalid.
    fn pull(&mut self) -> Result<Output>;

    /// Processes one complete input and returns the whole result.
    ///
    /// This is shorthand for [`push`][Compression::push], [`finish`][Compression::finish], and
    /// draining [`pull`][Compression::pull]. It ends the operation, so an implementation serves
    /// one call. Drive `pull` directly to keep memory bounded by the configured chunk size.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying engine fails or the input is invalid.
    fn process(mut self, input: BytesView) -> Result<BytesView>
    where
        Self: Sized,
    {
        self.push(input)?;
        self.finish();

        let mut collected = BytesBuf::new();
        while let Some(chunk) = self.pull()?.into_data() {
            collected.put_bytes(chunk);
        }

        Ok(collected.consume_all())
    }

    /// Compresses one complete input and returns the whole result.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying compression engine fails.
    fn compress(self, input: BytesView) -> Result<BytesView>
    where
        Self: Sized + Compression<Mode = Compress>,
    {
        self.process(input)
    }

    /// Decompresses one complete input and returns the whole result.
    ///
    /// # Errors
    ///
    /// Returns an error if the data is invalid, truncated, or exceeds the configured limits.
    fn decompress(self, input: BytesView) -> Result<BytesView>
    where
        Self: Sized + Compression<Mode = Decompress>,
    {
        self.process(input)
    }
}

/// Implements the shared trait for a format module's compressor and decompressor.
#[cfg(any(feature = "brotli", feature = "deflate", feature = "gzip", feature = "zlib", feature = "zstd"))]
macro_rules! impl_compression {
    ($($module:ident),+ $(,)?) => {
        $(
            impl Compression for crate::$module::Compressor {
                type Mode = Compress;

                fn push(&mut self, input: BytesView) -> Result<()> {
                    Self::push(self, input)
                }

                fn finish(&mut self) {
                    Self::finish(self);
                }

                fn pull(&mut self) -> Result<Output> {
                    Self::pull(self)
                }
            }

            impl Compression for crate::$module::Decompressor {
                type Mode = Decompress;

                fn push(&mut self, input: BytesView) -> Result<()> {
                    Self::push(self, input)
                }

                fn finish(&mut self) {
                    Self::finish(self);
                }

                fn pull(&mut self) -> Result<Output> {
                    Self::pull(self)
                }
            }
        )+
    };
}

#[cfg(feature = "brotli")]
impl_compression!(brotli);
#[cfg(feature = "deflate")]
impl_compression!(deflate);
#[cfg(feature = "gzip")]
impl_compression!(gzip);
#[cfg(feature = "zlib")]
impl_compression!(zlib);
#[cfg(feature = "zstd")]
impl_compression!(zstd);

impl<D> Compression for Box<dyn Compression<Mode = D>> {
    type Mode = D;

    fn push(&mut self, input: BytesView) -> Result<()> {
        (**self).push(input)
    }

    fn finish(&mut self) {
        (**self).finish();
    }

    fn pull(&mut self) -> Result<Output> {
        (**self).pull()
    }
}

#[cfg(all(test, feature = "gzip"))]
mod tests {
    use bytesbuf::mem::GlobalPool;

    use super::*;
    use crate::gzip;

    fn view(bytes: &[u8]) -> BytesView {
        BytesView::copied_from_slice(bytes, &GlobalPool::new())
    }

    #[test]
    fn round_trips_through_the_trait_alone() {
        let memory = GlobalPool::new();

        let mut compressor: Box<dyn Compression<Mode = Compress>> = Box::new(gzip::Compressor::new(memory.clone()));
        Compression::push(&mut *compressor, view(b"driven through the trait")).expect("push succeeds");
        Compression::finish(&mut *compressor);

        let mut collected = BytesBuf::new();
        while let Some(chunk) = Compression::pull(&mut *compressor).expect("pull succeeds").into_data() {
            collected.put_bytes(chunk);
        }

        let mut decompressor: Box<dyn Compression<Mode = Decompress>> = Box::new(gzip::Decompressor::new(memory));
        Compression::push(&mut *decompressor, collected.consume_all()).expect("push succeeds");
        Compression::finish(&mut *decompressor);

        let mut plain = BytesBuf::new();
        while let Some(chunk) = Compression::pull(&mut *decompressor).expect("pull succeeds").into_data() {
            plain.put_bytes(chunk);
        }

        assert_eq!(plain.consume_all().to_vec(), b"driven through the trait".to_vec());
    }

    #[test]
    fn trait_objects_are_send_sync_and_debug() {
        fn assert_send_sync<T: Send + Sync + ?Sized>(_: &T) {}

        let memory = GlobalPool::new();
        let compressor: Box<dyn Compression<Mode = Compress>> = Box::new(gzip::Compressor::new(memory.clone()));
        let decompressor: Box<dyn Compression<Mode = Decompress>> = Box::new(gzip::Decompressor::new(memory));

        assert_send_sync(&*compressor);
        assert_send_sync(&*decompressor);
        assert_send_sync(&gzip::Compressor::new(GlobalPool::new()));
        assert_send_sync(&gzip::Decompressor::new(GlobalPool::new()));
        assert!(format!("{compressor:?}").contains("Compressor"));
        assert!(format!("{decompressor:?}").contains("Decompressor"));
    }
}
