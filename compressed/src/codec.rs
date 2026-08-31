// Licensed under the MIT License.

use std::fmt;

use bytesbuf::BytesView;

use crate::error::Result;
use crate::gzip;
use crate::output::Output;

mod sealed {
    pub trait Sealed {}

    impl Sealed for crate::gzip::Encoder {}
    impl Sealed for crate::gzip::Decoder {}
}

/// A streaming compressor.
///
/// This is the contract every format's encoder satisfies, so callers can be generic over the
/// format. [`gzip::Encoder`] is the only implementation today.
///
/// The trait is sealed: further formats can be added, and the trait can gain the methods they
/// need, without breaking downstream code.
///
/// ```
/// use bytesbuf::BytesView;
/// use bytesbuf::mem::{GlobalPool, MemoryShared};
/// use compressed::{Encoder, Output, gzip};
///
/// /// Compresses a payload with whichever encoder it is handed.
/// fn encode(mut encoder: impl Encoder, input: BytesView) -> compressed::Result<BytesView> {
///     encoder.push(input)?;
///     encoder.finish();
///
///     let mut parts = Vec::new();
///     while let Some(chunk) = encoder.pull()?.into_data() {
///         parts.push(chunk);
///     }
///
///     Ok(BytesView::from_views(parts))
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
pub trait Encoder: sealed::Sealed + fmt::Debug + Send {
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
}

/// A streaming decompressor.
///
/// This is the contract every format's decoder satisfies, so callers can be generic over the
/// format. [`gzip::Decoder`] is the only implementation today.
///
/// The trait is sealed: further formats can be added, and the trait can gain the methods they
/// need, without breaking downstream code.
pub trait Decoder: sealed::Sealed + fmt::Debug + Send {
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
}

impl Encoder for gzip::Encoder {
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

impl Decoder for gzip::Decoder {
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

#[cfg(test)]
mod tests {
    use bytesbuf::mem::GlobalPool;

    use super::*;

    fn view(bytes: &[u8]) -> BytesView {
        BytesView::copied_from_slice(bytes, &GlobalPool::new())
    }

    #[test]
    fn round_trips_through_the_traits_alone() {
        let memory = GlobalPool::new();

        let mut encoder: Box<dyn Encoder> = Box::new(gzip::Encoder::new(memory.clone()));
        Encoder::push(&mut *encoder, view(b"driven through the trait")).expect("push succeeds");
        Encoder::finish(&mut *encoder);

        let mut parts = Vec::new();
        while let Some(chunk) = Encoder::pull(&mut *encoder).expect("pull succeeds").into_data() {
            parts.push(chunk);
        }

        let mut decoder: Box<dyn Decoder> = Box::new(gzip::Decoder::new(memory));
        Decoder::push(&mut *decoder, BytesView::from_views(parts)).expect("push succeeds");
        Decoder::finish(&mut *decoder);

        let mut plain = Vec::new();
        while let Some(chunk) = Decoder::pull(&mut *decoder).expect("pull succeeds").into_data() {
            plain.push(chunk);
        }

        assert_eq!(BytesView::from_views(plain).to_vec(), b"driven through the trait".to_vec());
    }

    #[test]
    fn trait_objects_are_send_and_debug() {
        fn assert_send<T: Send + ?Sized>(_: &T) {}

        let memory = GlobalPool::new();
        let encoder: Box<dyn Encoder> = Box::new(gzip::Encoder::new(memory.clone()));
        let decoder: Box<dyn Decoder> = Box::new(gzip::Decoder::new(memory));

        assert_send(&*encoder);
        assert_send(&*decoder);
        assert!(format!("{encoder:?}").contains("Encoder"));
        assert!(format!("{decoder:?}").contains("Decoder"));
    }
}
