// Licensed under the MIT License.

use std::fmt;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytesbuf::BytesView;
#[cfg(any(feature = "brotli", feature = "deflate", feature = "gzip", feature = "zlib", feature = "zstd"))]
use bytesbuf::mem::MemoryShared;
use futures_core::Stream;
use pin_project_lite::pin_project;

use crate::codec::{Decoder, Encoder};
use crate::error::{Error, Result};
use crate::output::Output;

/// The push/pull surface both directions share, so the adapters need only one driver.
///
/// Private, and erased behind a box inside the stream types. That keeps the streams at a single
/// type parameter — `CompressStream<S>` rather than `CompressStream<S, E>` — so callers never have
/// to name the codec type, while `Encoder` and `Decoder` still keep the two directions from being
/// mixed up at the constructor.
trait PushPull: fmt::Debug + Send {
    fn push(&mut self, input: BytesView) -> Result<()>;
    fn finish(&mut self);
    fn pull(&mut self) -> Result<Output>;
}

#[derive(Debug)]
struct AsEncoder<E>(E);

impl<E: Encoder> PushPull for AsEncoder<E> {
    fn push(&mut self, input: BytesView) -> Result<()> {
        Encoder::push(&mut self.0, input)
    }

    fn finish(&mut self) {
        Encoder::finish(&mut self.0);
    }

    fn pull(&mut self) -> Result<Output> {
        Encoder::pull(&mut self.0)
    }
}

#[derive(Debug)]
struct AsDecoder<D>(D);

impl<D: Decoder> PushPull for AsDecoder<D> {
    fn push(&mut self, input: BytesView) -> Result<()> {
        Decoder::push(&mut self.0, input)
    }

    fn finish(&mut self) {
        Decoder::finish(&mut self.0);
    }

    fn pull(&mut self) -> Result<Output> {
        Decoder::pull(&mut self.0)
    }
}

/// Drives a codec from a source stream.
///
/// The source is polled only when the codec has nothing left to give, so a slow consumer never
/// causes unbounded buffering.
///
/// `finished` latches once the stream has yielded its last item. Without it, a failing codec would
/// report the same error on every subsequent poll, and a caller that collects the stream would
/// accumulate errors until it ran out of memory.
fn poll_codec<S, E>(
    mut source: Pin<&mut S>,
    codec: &mut (impl PushPull + ?Sized),
    finished: &mut bool,
    cx: &mut Context<'_>,
) -> Poll<Option<Result<BytesView>>>
where
    S: Stream<Item = std::result::Result<BytesView, E>>,
    E: std::error::Error + Send + Sync + 'static,
{
    if *finished {
        return Poll::Ready(None);
    }

    loop {
        match codec.pull() {
            Err(error) => {
                *finished = true;
                return Poll::Ready(Some(Err(error)));
            }
            Ok(Output::Data(data)) => return Poll::Ready(Some(Ok(data))),
            Ok(Output::Done) => {
                *finished = true;
                return Poll::Ready(None);
            }
            Ok(Output::NeedInput) => match source.as_mut().poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => codec.finish(),
                Poll::Ready(Some(Ok(chunk))) => {
                    if let Err(error) = codec.push(chunk) {
                        *finished = true;
                        return Poll::Ready(Some(Err(error)));
                    }
                }
                Poll::Ready(Some(Err(error))) => {
                    *finished = true;
                    return Poll::Ready(Some(Err(Error::source(error))));
                }
            },
        }
    }
}

pin_project! {
    /// Compresses a stream of [`BytesView`] values into a stream of compressed chunks.
    ///
    /// Construct it for a specific format with a named constructor such as
    /// [`CompressStream::gzip`], or hand it a pre-configured encoder with [`CompressStream::new`].
    ///
    /// The stream ends after its first error rather than reporting the same failure repeatedly.
    ///
    /// ```
    /// use bytesbuf::BytesView;
    /// use bytesbuf::mem::GlobalPool;
    /// use compressed::CompressStream;
    /// use futures::StreamExt;
    /// use futures::stream;
    ///
    /// # futures::executor::block_on(async {
    /// let memory = GlobalPool::new();
    /// let source = stream::iter(vec![
    ///     Ok::<_, std::io::Error>(BytesView::copied_from_slice(b"first ", &memory)),
    ///     Ok(BytesView::copied_from_slice(b"second", &memory)),
    /// ]);
    ///
    /// let chunks: Vec<_> = CompressStream::gzip(source, memory).collect().await;
    /// let gzip = BytesView::from_views(chunks.into_iter().map(|c| c.unwrap()));
    ///
    /// assert_eq!(gzip.range(0..2).to_vec(), vec![0x1f, 0x8b]);
    /// # });
    /// ```
    #[derive(Debug)]
    pub struct CompressStream<S> {
        #[pin]
        source: S,
        encoder: Box<dyn PushPull>,
        finished: bool,
    }
}

impl<S> CompressStream<S> {
    /// Compresses `source` as gzip at [`Level::DEFAULT`][crate::Level::DEFAULT].
    ///
    /// To choose a compression level or output chunk size, build an encoder with its `builder` and
    /// pass it to [`CompressStream::new`]. For a format chosen at runtime, pass
    /// [`Format::encoder`][crate::Format::encoder].
    #[cfg(feature = "gzip")]
    #[must_use]
    pub fn gzip(source: S, memory: impl MemoryShared) -> Self {
        Self::new(source, crate::gzip::Encoder::new(memory))
    }

    /// Compresses `source` as zlib at [`Level::DEFAULT`][crate::Level::DEFAULT].
    #[cfg(feature = "zlib")]
    #[must_use]
    pub fn zlib(source: S, memory: impl MemoryShared) -> Self {
        Self::new(source, crate::zlib::Encoder::new(memory))
    }

    /// Compresses `source` as raw deflate at [`Level::DEFAULT`][crate::Level::DEFAULT].
    #[cfg(feature = "deflate")]
    #[must_use]
    pub fn deflate(source: S, memory: impl MemoryShared) -> Self {
        Self::new(source, crate::deflate::Encoder::new(memory))
    }

    /// Compresses `source` as brotli at [`Level::DEFAULT`][crate::Level::DEFAULT].
    #[cfg(feature = "brotli")]
    #[must_use]
    pub fn brotli(source: S, memory: impl MemoryShared) -> Self {
        Self::new(source, crate::brotli::Encoder::new(memory))
    }

    /// Compresses `source` as zstd at [`Level::DEFAULT`][crate::Level::DEFAULT].
    #[cfg(feature = "zstd")]
    #[must_use]
    pub fn zstd(source: S, memory: impl MemoryShared) -> Self {
        Self::new(source, crate::zstd::Encoder::new(memory))
    }

    /// Compresses `source` with a pre-configured encoder.
    #[must_use]
    pub fn new(source: S, encoder: impl Encoder + 'static) -> Self {
        Self {
            source,
            encoder: Box::new(AsEncoder(encoder)),
            finished: false,
        }
    }
}

impl<S, E> Stream for CompressStream<S>
where
    S: Stream<Item = std::result::Result<BytesView, E>>,
    E: std::error::Error + Send + Sync + 'static,
{
    type Item = Result<BytesView>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();
        poll_codec(this.source, this.encoder.as_mut(), this.finished, cx)
    }
}

pin_project! {
    /// Decompresses a stream of compressed chunks into a stream of plaintext [`BytesView`] values.
    ///
    /// Construct it for a specific format with a named constructor such as
    /// [`DecompressStream::gzip`], or hand it a pre-configured decoder with
    /// [`DecompressStream::new`].
    ///
    /// The stream ends after its first error rather than reporting the same failure repeatedly.
    ///
    /// # Security
    ///
    /// [`DecompressStream::gzip`] applies
    /// [`DecompressionLimits::new()`][crate::DecompressionLimits::new()]. Build a decoder with
    /// [`gzip::Decoder::builder`][crate::gzip::Decoder::builder] and pass it to [`DecompressStream::new`] to tighten the limits for
    /// untrusted sources.
    ///
    /// ```
    /// use bytesbuf::BytesView;
    /// use bytesbuf::mem::GlobalPool;
    /// use compressed::{DecompressStream, gzip};
    /// use futures::StreamExt;
    /// use futures::stream;
    ///
    /// # futures::executor::block_on(async {
    /// let memory = GlobalPool::new();
    /// let encoded = gzip::compress(
    ///     BytesView::copied_from_slice(b"payload", &memory),
    ///     memory.clone(),
    /// ).unwrap();
    ///
    /// // Deliver the gzip stream one byte at a time, the worst case for a decoder.
    /// let source = stream::iter(
    ///     (0..encoded.len())
    ///         .map(|i| Ok::<_, std::io::Error>(encoded.range(i..i + 1)))
    ///         .collect::<Vec<_>>(),
    /// );
    ///
    /// let chunks: Vec<_> = DecompressStream::gzip(source, memory).collect().await;
    /// let plain = BytesView::from_views(chunks.into_iter().map(|c| c.unwrap()));
    ///
    /// assert_eq!(plain.to_vec(), b"payload".to_vec());
    /// # });
    /// ```
    #[derive(Debug)]
    pub struct DecompressStream<S> {
        #[pin]
        source: S,
        decoder: Box<dyn PushPull>,
        finished: bool,
    }
}

impl<S> DecompressStream<S> {
    /// Decompresses a gzip `source` with
    /// [`DecompressionLimits::new()`][crate::DecompressionLimits::new()].
    ///
    /// For a format chosen at runtime, pass [`Format::decoder`][crate::Format::decoder] to
    /// [`DecompressStream::new`].
    #[cfg(feature = "gzip")]
    #[must_use]
    pub fn gzip(source: S, memory: impl MemoryShared) -> Self {
        Self::new(source, crate::gzip::Decoder::new(memory))
    }

    /// Decompresses a zlib `source` with
    /// [`DecompressionLimits::new()`][crate::DecompressionLimits::new()].
    #[cfg(feature = "zlib")]
    #[must_use]
    pub fn zlib(source: S, memory: impl MemoryShared) -> Self {
        Self::new(source, crate::zlib::Decoder::new(memory))
    }

    /// Decompresses a raw deflate `source` with
    /// [`DecompressionLimits::new()`][crate::DecompressionLimits::new()].
    #[cfg(feature = "deflate")]
    #[must_use]
    pub fn deflate(source: S, memory: impl MemoryShared) -> Self {
        Self::new(source, crate::deflate::Decoder::new(memory))
    }

    /// Decompresses a brotli `source` with
    /// [`DecompressionLimits::new()`][crate::DecompressionLimits::new()].
    #[cfg(feature = "brotli")]
    #[must_use]
    pub fn brotli(source: S, memory: impl MemoryShared) -> Self {
        Self::new(source, crate::brotli::Decoder::new(memory))
    }

    /// Decompresses `source` with a pre-configured decoder.
    #[must_use]
    pub fn new(source: S, decoder: impl Decoder + 'static) -> Self {
        Self {
            source,
            decoder: Box::new(AsDecoder(decoder)),
            finished: false,
        }
    }
}

impl<S, E> Stream for DecompressStream<S>
where
    S: Stream<Item = std::result::Result<BytesView, E>>,
    E: std::error::Error + Send + Sync + 'static,
{
    type Item = Result<BytesView>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();
        poll_codec(this.source, this.decoder.as_mut(), this.finished, cx)
    }
}

#[cfg(all(test, feature = "gzip"))]
mod tests {
    use bytesbuf::mem::GlobalPool;
    use futures::executor::block_on;
    use futures::{StreamExt, stream};

    use super::*;
    use crate::{DecompressionLimits, Level, gzip};

    fn view(bytes: &[u8]) -> BytesView {
        BytesView::copied_from_slice(bytes, &GlobalPool::new())
    }

    fn ok_stream(chunks: Vec<BytesView>) -> impl Stream<Item = std::result::Result<BytesView, std::io::Error>> {
        stream::iter(chunks.into_iter().map(Ok))
    }

    fn collect(stream: impl Stream<Item = Result<BytesView>>) -> Result<BytesView> {
        block_on(async {
            let chunks: Vec<_> = stream.collect().await;
            let mut parts = Vec::with_capacity(chunks.len());
            for chunk in chunks {
                parts.push(chunk?);
            }
            Ok(BytesView::from_views(parts))
        })
    }

    #[test]
    fn round_trips_through_both_adapters() {
        let memory = GlobalPool::new();
        let payload = b"streaming round trip ".repeat(500);

        let source = ok_stream(payload.chunks(97).map(view).collect());
        let gzip = collect(CompressStream::gzip(source, memory.clone())).expect("compression succeeds");

        let plain = collect(DecompressStream::gzip(ok_stream(vec![gzip]), memory)).expect("decompression succeeds");

        assert_eq!(plain.to_vec(), payload);
    }

    #[test]
    fn compresses_an_empty_source() {
        let source = ok_stream(Vec::new());
        let gzip = collect(CompressStream::gzip(source, GlobalPool::new())).expect("compression succeeds");

        assert_eq!(gzip.range(0..2).to_vec(), vec![0x1f, 0x8b]);
    }

    #[test]
    fn decompresses_a_byte_at_a_time() {
        let memory = GlobalPool::new();
        let encoded = crate::gzip::compress(view(b"one byte at a time"), memory.clone()).expect("compression succeeds");
        let single_bytes = (0..encoded.len()).map(|i| encoded.range(i..=i)).collect();

        let plain = collect(DecompressStream::gzip(ok_stream(single_bytes), memory)).expect("decompression succeeds");

        assert_eq!(plain.to_vec(), b"one byte at a time".to_vec());
    }

    #[test]
    fn reports_a_failing_source_as_a_source_error() {
        let failing = stream::iter(vec![Err(std::io::Error::other("transport died"))]);

        let error = collect(CompressStream::gzip(failing, GlobalPool::new())).expect_err("the source failure surfaces");

        assert!(error.is_source(), "got {error}");
        assert_eq!(
            std::error::Error::source(&error).map(ToString::to_string),
            Some("transport died".to_owned()),
            "the original failure should remain reachable"
        );
    }

    #[test]
    fn ends_after_the_first_error_instead_of_repeating_it() {
        // A stream that keeps yielding the same error is unbounded: a caller that collects it
        // accumulates errors until it runs out of memory.
        let source = ok_stream(vec![view(b"this is not gzip")]);
        let mut stream = Box::pin(DecompressStream::gzip(source, GlobalPool::new()));

        block_on(async {
            let first = stream.next().await.expect("an error is reported");
            assert!(first.expect_err("the data is invalid").is_corrupt_data());

            assert!(stream.next().await.is_none(), "the stream must end after an error");
            assert!(stream.next().await.is_none(), "and stay ended");
        });
    }

    #[test]
    fn stays_ended_after_completion() {
        let memory = GlobalPool::new();
        let gzip = crate::gzip::compress(view(b"done"), memory.clone()).expect("compression succeeds");
        let mut stream = Box::pin(DecompressStream::gzip(ok_stream(vec![gzip]), memory));

        block_on(async {
            while stream.next().await.is_some() {}
            assert!(stream.next().await.is_none(), "a completed stream stays ended");
        });
    }

    #[test]
    fn reports_corrupt_input_from_the_decompress_adapter() {
        let source = ok_stream(vec![view(b"this is not gzip")]);

        let error = collect(DecompressStream::gzip(source, GlobalPool::new())).expect_err("bad data is rejected");

        assert!(error.is_corrupt_data(), "got {error}");
    }

    #[test]
    fn honours_a_pre_configured_decoder() {
        let memory = GlobalPool::new();
        let gzip = crate::gzip::compress(view(&vec![0_u8; 4 * 1024 * 1024]), memory.clone()).expect("compression succeeds");

        let decoder = gzip::Decoder::builder()
            .limits(DecompressionLimits::new().with_max_output_len(1024))
            .build(memory);

        let error = collect(DecompressStream::new(ok_stream(vec![gzip]), decoder)).expect_err("the cap fires");

        assert!(error.is_limit_exceeded(), "got {error}");
    }

    #[test]
    fn honours_a_pre_configured_encoder() {
        let memory = GlobalPool::new();
        let payload = b"the quick brown fox ".repeat(400);

        let encoder = gzip::Encoder::builder().level(Level::BEST).build(memory.clone());
        let gzip = collect(CompressStream::new(ok_stream(vec![view(&payload)]), encoder)).expect("compression succeeds");

        let plain = collect(DecompressStream::gzip(ok_stream(vec![gzip]), memory)).expect("decompression succeeds");

        assert_eq!(plain.to_vec(), payload);
    }

    #[test]
    fn tolerates_empty_chunks_from_the_source() {
        let memory = GlobalPool::new();
        let source = ok_stream(vec![BytesView::new(), view(b"data"), BytesView::new()]);

        let gzip = collect(CompressStream::gzip(source, memory.clone())).expect("compression succeeds");
        let plain = collect(DecompressStream::gzip(ok_stream(vec![gzip]), memory)).expect("decompression succeeds");

        assert_eq!(plain.to_vec(), b"data".to_vec());
    }

    #[test]
    fn waits_for_a_pending_source() {
        // Exercises the `Poll::Pending` arm: the source stalls once before ending.
        let mut stalled = false;
        let source = stream::poll_fn(move |cx| -> Poll<Option<std::result::Result<BytesView, std::io::Error>>> {
            if stalled {
                return Poll::Ready(None);
            }

            stalled = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        });

        let gzip = collect(CompressStream::gzip(source, GlobalPool::new())).expect("compression succeeds");

        assert_eq!(gzip.range(0..2).to_vec(), vec![0x1f, 0x8b]);
    }

    #[test]
    fn streams_are_send_so_they_can_cross_task_boundaries() {
        // `!Send` is infectious: a stream that cannot move between tasks is unusable in most async
        // runtimes. The `Send` supertrait on `Encoder`/`Decoder` is what makes the boxed codec Send.
        fn assert_send<T: Send>(_: &T) {}

        let memory = GlobalPool::new();
        assert_send(&CompressStream::gzip(ok_stream(Vec::new()), memory.clone()));
        assert_send(&DecompressStream::gzip(ok_stream(Vec::new()), memory));
    }

    #[test]
    fn debug_is_available_for_diagnostics() {
        let empty = stream::iter(Vec::<std::result::Result<BytesView, std::io::Error>>::new());
        let stream = CompressStream::gzip(empty, GlobalPool::new());

        assert!(format!("{stream:?}").contains("CompressStream"));
    }
}
