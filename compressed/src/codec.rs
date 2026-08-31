// Licensed under the MIT License.

use std::fmt;

use bytesbuf::{BytesBuf, BytesView};

use crate::error::Result;
use crate::output::Output;

mod sealed {
    extern crate alloc;

    pub trait Sealed {}

    #[cfg(feature = "brotli")]
    impl Sealed for crate::brotli::Encoder {}
    #[cfg(feature = "brotli")]
    impl Sealed for crate::brotli::Decoder {}
    #[cfg(feature = "deflate")]
    impl Sealed for crate::deflate::Encoder {}
    #[cfg(feature = "deflate")]
    impl Sealed for crate::deflate::Decoder {}
    #[cfg(feature = "gzip")]
    impl Sealed for crate::gzip::Encoder {}
    #[cfg(feature = "gzip")]
    impl Sealed for crate::gzip::Decoder {}
    #[cfg(feature = "zlib")]
    impl Sealed for crate::zlib::Encoder {}
    #[cfg(feature = "zlib")]
    impl Sealed for crate::zlib::Decoder {}
    #[cfg(feature = "zstd")]
    impl Sealed for crate::zstd::Encoder {}
    #[cfg(feature = "zstd")]
    impl Sealed for crate::zstd::Decoder {}

    impl Sealed for alloc::boxed::Box<dyn super::Encoder> {}
    impl Sealed for alloc::boxed::Box<dyn super::Decoder> {}
}

/// A streaming compressor.
///
/// This is the contract every format's encoder satisfies, so callers can be generic over the
/// format, whichever of them a build enables. A `Box<dyn Encoder>` is itself an `Encoder`, so a
/// format chosen at runtime with [`Format::encoder`][crate::format::Format::encoder] fits anywhere a
/// concrete encoder does.
///
/// The trait is sealed: further formats can be added, and the trait can gain the methods they
/// need, without breaking downstream code. Every implementation is `Send + Sync`, so a
/// `Box<dyn Encoder>` can be shared as well as moved between threads.
///
/// # Examples
///
/// ```
/// use bytesbuf::BytesView;
/// use bytesbuf::mem::{GlobalPool, MemoryShared};
/// use compressed::{Encoder, Output, gzip};
///
/// /// Compresses a payload with whichever encoder it is handed.
/// fn encode(mut encoder: impl Encoder, input: BytesView) -> compressed::Result<BytesView> {
///     encoder.encode(input)
/// }
///
/// let memory = GlobalPool::new();
/// let encoded = encode(
///     gzip::Encoder::new(memory.clone()),
///     BytesView::copied_from_slice(b"format agnostic", &memory),
/// )?;
///
/// assert_eq!(encoded.range(0..2).to_vec(), vec![0x1f, 0x8b]);
/// # Ok::<(), compressed::Error>(())
/// ```
pub trait Encoder: sealed::Sealed + fmt::Debug + Send + Sync {
    /// Supplies more uncompressed input.
    ///
    /// # Errors
    ///
    /// Returns an error if input is still pending or the encoder has been finished.
    fn push(&mut self, input: BytesView) -> Result<()>;

    /// Signals that no further input will be supplied.
    fn finish(&mut self);

    /// Produces the next chunk of compressed output.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying compression engine fails.
    fn pull(&mut self) -> Result<Output>;

    /// Compresses one complete input and returns the whole result.
    ///
    /// The shorthand for [`push`][Encoder::push], [`finish`][Encoder::finish] and draining
    /// [`pull`][Encoder::pull]. It ends the stream, so an encoder serves one call; build another
    /// for the next payload, from a [`Pool`][crate::Pool] if the setup cost matters.
    ///
    /// Prefer a free function such as [`gzip::compress`][crate::gzip::compress] when the default
    /// configuration will do. Those build a fresh encoder per call and so can be called
    /// repeatedly, which this deliberately cannot; this exists for an encoder configured with a
    /// level, a pool or a chunk size.
    ///
    /// This buffers the whole result, so peak memory follows its size. Drive
    /// [`pull`][Encoder::pull] directly to keep memory bounded by the chunk size instead.
    ///
    /// Taking `self` by value is what makes "one call per encoder" a compile error rather than a
    /// runtime one. `Self: Sized` keeps the trait object safe, and a `Box<dyn Encoder>` is itself
    /// sized, so a runtime-selected encoder can still be consumed this way.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying compression engine fails.
    ///
    /// # Examples
    ///
    /// An encoder is spent once it has encoded, so a second call does not compile:
    ///
    /// ```compile_fail
    /// use bytesbuf::BytesView;
    /// use bytesbuf::mem::GlobalPool;
    /// use compressed::{Encoder, gzip};
    ///
    /// let memory = GlobalPool::new();
    /// let input = BytesView::copied_from_slice(b"payload", &memory);
    /// let encoder = gzip::Encoder::new(memory);
    ///
    /// encoder.encode(input.clone())?;
    /// encoder.encode(input)?;
    /// # Ok::<(), compressed::Error>(())
    /// ```
    fn encode(mut self, input: BytesView) -> Result<BytesView>
    where
        Self: Sized,
    {
        self.push(input)?;
        self.finish();

        // Appending a view to a `BytesBuf` is zero-copy, so the chunks are joined without an
        // intermediate allocation.
        let mut collected = BytesBuf::new();
        while let Some(chunk) = self.pull()?.into_data() {
            collected.put_bytes(chunk);
        }

        Ok(collected.consume_all())
    }
}

/// A streaming decompressor.
///
/// This is the contract every format's decoder satisfies, so callers can be generic over the
/// format, whichever of them a build enables. A `Box<dyn Decoder>` is itself a `Decoder`, so a
/// format chosen at runtime with [`Format::decoder`][crate::format::Format::decoder] fits anywhere a
/// concrete decoder does.
///
/// The trait is sealed: further formats can be added, and the trait can gain the methods they
/// need, without breaking downstream code.
pub trait Decoder: sealed::Sealed + fmt::Debug + Send + Sync {
    /// Supplies more compressed input.
    ///
    /// # Errors
    ///
    /// Returns an error if input is still pending or the decoder has been finished.
    fn push(&mut self, input: BytesView) -> Result<()>;

    /// Signals that no further input will be supplied.
    fn finish(&mut self);

    /// Produces the next chunk of decompressed output.
    ///
    /// # Errors
    ///
    /// Returns an error if the data is invalid, truncated, or exceeds the configured limits.
    fn pull(&mut self) -> Result<Output>;

    /// Decompresses one complete stream and returns the whole result.
    ///
    /// The shorthand for [`push`][Decoder::push], [`finish`][Decoder::finish] and draining
    /// [`pull`][Decoder::pull]. It ends the stream, so a decoder serves one call.
    ///
    /// Prefer a free function such as [`gzip::decompress`][crate::gzip::decompress] when the
    /// default limits will do; this exists for a decoder configured with its own limits, a pool or
    /// a chunk size.
    ///
    /// This buffers the entire result, so it suits data whose size is already known and trusted.
    /// Against a hostile stream, drive [`pull`][Decoder::pull] and stop early instead: the
    /// configured limits still apply here, but only once the work has been done.
    ///
    /// Taking `self` by value is what makes "one call per decoder" a compile error rather than a
    /// runtime one. `Self: Sized` keeps the trait object safe, and a `Box<dyn Decoder>` is itself
    /// sized, so a runtime-selected decoder can still be consumed this way.
    ///
    /// # Errors
    ///
    /// Returns an error if the data is invalid, truncated, or exceeds the configured limits.
    fn decode(mut self, input: BytesView) -> Result<BytesView>
    where
        Self: Sized,
    {
        self.push(input)?;
        self.finish();

        // Appending a view to a `BytesBuf` is zero-copy, so the chunks are joined without an
        // intermediate allocation.
        let mut collected = BytesBuf::new();
        while let Some(chunk) = self.pull()?.into_data() {
            collected.put_bytes(chunk);
        }

        Ok(collected.consume_all())
    }
}

/// Forwards the trait methods to a format module's inherent methods.
#[cfg(any(feature = "brotli", feature = "deflate", feature = "gzip", feature = "zlib", feature = "zstd"))]
macro_rules! impl_codec_traits {
    ($($module:ident),+ $(,)?) => {
        $(
            impl Encoder for crate::$module::Encoder {
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

            impl Decoder for crate::$module::Decoder {
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
impl_codec_traits!(brotli);
#[cfg(feature = "deflate")]
impl_codec_traits!(deflate);
#[cfg(feature = "gzip")]
impl_codec_traits!(gzip);
#[cfg(feature = "zlib")]
impl_codec_traits!(zlib);
#[cfg(feature = "zstd")]
impl_codec_traits!(zstd);

// A boxed codec is itself a codec, so anything that accepts `impl Encoder` also accepts the
// runtime-selected `Box<dyn Encoder>` that `Format::encoder` returns.
impl Encoder for Box<dyn Encoder> {
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

impl Decoder for Box<dyn Decoder> {
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
    fn round_trips_through_the_traits_alone() {
        let memory = GlobalPool::new();

        let mut encoder: Box<dyn Encoder> = Box::new(gzip::Encoder::new(memory.clone()));
        Encoder::push(&mut *encoder, view(b"driven through the trait")).expect("push succeeds");
        Encoder::finish(&mut *encoder);

        let mut collected = BytesBuf::new();
        while let Some(chunk) = Encoder::pull(&mut *encoder).expect("pull succeeds").into_data() {
            collected.put_bytes(chunk);
        }

        let mut decoder: Box<dyn Decoder> = Box::new(gzip::Decoder::new(memory));
        Decoder::push(&mut *decoder, collected.consume_all()).expect("push succeeds");
        Decoder::finish(&mut *decoder);

        let mut plain = BytesBuf::new();
        while let Some(chunk) = Decoder::pull(&mut *decoder).expect("pull succeeds").into_data() {
            plain.put_bytes(chunk);
        }

        assert_eq!(plain.consume_all().to_vec(), b"driven through the trait".to_vec());
    }

    #[test]
    fn trait_objects_are_send_and_debug() {
        fn assert_send<T: Send + ?Sized>(_: &T) {}
        fn assert_send_sync<T: Send + Sync + ?Sized>(_: &T) {}

        let memory = GlobalPool::new();
        let encoder: Box<dyn Encoder> = Box::new(gzip::Encoder::new(memory.clone()));
        let decoder: Box<dyn Decoder> = Box::new(gzip::Decoder::new(memory));

        assert_send(&*encoder);
        assert_send(&*decoder);

        // Every concrete codec is `Sync`, so the trait objects must be too: `!Sync` would stop a
        // boxed codec being shared behind an `Arc`, and adding `Sync` later is a breaking change.
        assert_send_sync(&*encoder);
        assert_send_sync(&*decoder);
        assert_send_sync(&gzip::Encoder::new(GlobalPool::new()));
        assert_send_sync(&gzip::Decoder::new(GlobalPool::new()));
        assert!(format!("{encoder:?}").contains("Encoder"));
        assert!(format!("{decoder:?}").contains("Decoder"));
    }
}
