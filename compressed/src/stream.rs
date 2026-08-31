// Licensed under the MIT License.

//! Compression and decompression as [`futures_core::Stream`] adapters.
//!
//! [`Compress`] and [`Decompress`] wrap any stream of byte sequences and yield the converted
//! chunks as they become available, so a body of any size passes through in bounded memory.
//! Requires the `futures-stream` cargo feature.
//!
//! Both are generic only over the source stream. The codec is erased internally, so a format
//! chosen at run time needs no extra type parameter, and the source's error type is folded into
//! [`Error`] and retrievable through [`std::error::Error::source`].

use std::fmt;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytesbuf::BytesView;
use futures_core::Stream;
use pin_project_lite::pin_project;

use crate::codec::{Decoder, Encoder};
use crate::error::{Error, Result};
use crate::output::Output;

/// The push/pull surface both directions share, so one poll driver serves both adapters.
///
/// [`Encoder`] and [`Decoder`] are separate traits with no common supertrait, which is what keeps
/// the two directions from being mixed up at a constructor. This private trait re-unites them for
/// the one place that genuinely does not care which it is holding.
trait PushPull: fmt::Debug + Send {
    fn push(&mut self, input: BytesView) -> Result<()>;
    fn finish(&mut self);
    fn pull(&mut self) -> Result<Output>;
}

impl PushPull for Box<dyn Encoder> {
    fn push(&mut self, input: BytesView) -> Result<()> {
        Encoder::push(self, input)
    }

    fn finish(&mut self) {
        Encoder::finish(self);
    }

    fn pull(&mut self) -> Result<Output> {
        Encoder::pull(self)
    }
}

impl PushPull for Box<dyn Decoder> {
    fn push(&mut self, input: BytesView) -> Result<()> {
        Decoder::push(self, input)
    }

    fn finish(&mut self) {
        Decoder::finish(self);
    }

    fn pull(&mut self) -> Result<Output> {
        Decoder::pull(self)
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
    /// Hand [`Compress::new`] any [`Encoder`], whether from a format module or from
    /// [`Format::encoder`][crate::format::Format::encoder] for a format chosen at run time.
    ///
    /// The stream ends after its first error rather than reporting the same failure repeatedly.
    ///
    /// # Examples
    ///
    /// ```
    /// use bytesbuf::BytesView;
    /// use bytesbuf::mem::GlobalPool;
    /// use compressed::gzip;
    /// use compressed::stream::Compress;
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
    /// let chunks: Vec<_> = Compress::new(source, gzip::Encoder::new(memory)).collect().await;
    /// let gzip = BytesView::from_views(chunks.into_iter().map(|c| c.unwrap()));
    ///
    /// assert_eq!(gzip.range(0..2).to_vec(), vec![0x1f, 0x8b]);
    /// # });
    /// ```
    #[derive(Debug)]
    pub struct Compress<S> {
        #[pin]
        source: S,
        encoder: Box<dyn Encoder>,
        finished: bool,
    }
}

impl<S> Compress<S> {
    /// Compresses `source` with `encoder`.
    ///
    /// Build the encoder with its format's constructor or `builder`, or with
    /// [`Format::encoder`][crate::format::Format::encoder] for a format chosen at run time. Every
    /// format reaches the adapter the same way, so nothing here has to change when one is added.
    #[must_use]
    pub fn new(source: S, encoder: impl Encoder + 'static) -> Self {
        Self {
            source,
            encoder: Box::new(encoder),
            finished: false,
        }
    }
}

impl<S, E> Stream for Compress<S>
where
    S: Stream<Item = std::result::Result<BytesView, E>>,
    E: std::error::Error + Send + Sync + 'static,
{
    type Item = Result<BytesView>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();
        poll_codec(this.source, this.encoder, this.finished, cx)
    }
}

pin_project! {
    /// Decompresses a stream of compressed chunks into a stream of plaintext [`BytesView`] values.
    ///
    /// Hand [`Decompress::new`] any [`Decoder`], whether from a format module or from
    /// [`Format::decoder`][crate::format::Format::decoder] for a format chosen at run time.
    ///
    /// The stream ends after its first error rather than reporting the same failure repeatedly.
    ///
    /// # Security
    ///
    /// A decoder built with its format's `new` applies
    /// [`DecompressionLimits::new()`][crate::DecompressionLimits::new()], which bounds the
    /// expansion ratio but not the total output. For an untrusted source, build the decoder with
    /// its `builder` and set a limit the caller can actually afford before handing it over.
    ///
    /// # Examples
    ///
    /// ```
    /// use bytesbuf::BytesView;
    /// use bytesbuf::mem::GlobalPool;
    /// use compressed::gzip;
    /// use compressed::stream::Decompress;
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
    /// let chunks: Vec<_> = Decompress::new(source, gzip::Decoder::new(memory)).collect().await;
    /// let plain = BytesView::from_views(chunks.into_iter().map(|c| c.unwrap()));
    ///
    /// assert_eq!(plain.to_vec(), b"payload".to_vec());
    /// # });
    /// ```
    #[derive(Debug)]
    pub struct Decompress<S> {
        #[pin]
        source: S,
        decoder: Box<dyn Decoder>,
        finished: bool,
    }
}

impl<S> Decompress<S> {
    /// Decompresses `source` with `decoder`.
    ///
    /// Build the decoder with its format's constructor or `builder`, or with
    /// [`Format::decoder`][crate::format::Format::decoder] for a format chosen at run time. Every
    /// format reaches the adapter the same way, so nothing here has to change when one is added.
    #[must_use]
    pub fn new(source: S, decoder: impl Decoder + 'static) -> Self {
        Self {
            source,
            decoder: Box::new(decoder),
            finished: false,
        }
    }
}

impl<S, E> Stream for Decompress<S>
where
    S: Stream<Item = std::result::Result<BytesView, E>>,
    E: std::error::Error + Send + Sync + 'static,
{
    type Item = Result<BytesView>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();
        poll_codec(this.source, this.decoder, this.finished, cx)
    }
}

#[cfg(all(test, feature = "gzip"))]
mod tests {
    use bytesbuf::BytesBuf;
    use bytesbuf::mem::GlobalPool;
    use futures::executor::block_on;
    use futures::{StreamExt, stream};

    use super::*;
    use crate::format::Format;
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
            let mut collected = BytesBuf::new();
            for chunk in chunks {
                collected.put_bytes(chunk?);
            }
            Ok(collected.consume_all())
        })
    }

    #[test]
    fn round_trips_through_both_adapters() {
        let memory = GlobalPool::new();
        let payload = b"streaming round trip ".repeat(500);

        let source = ok_stream(payload.chunks(97).map(view).collect());
        let gzip = collect(Compress::new(source, gzip::Encoder::new(memory.clone()))).expect("compression succeeds");

        let plain = collect(Decompress::new(ok_stream(vec![gzip]), gzip::Decoder::new(memory))).expect("decompression succeeds");

        assert_eq!(plain.to_vec(), payload);
    }

    /// Every format must offer both halves of the pair.
    ///
    /// A format that gains a `Compress` constructor but not the matching `Decompress`
    /// one leaves callers able to write a stream they cannot read back, and the gap is invisible
    /// until someone reaches for it. This drives each pair end to end so the omission cannot
    /// survive a build.
    #[test]
    fn every_format_round_trips_through_the_adapters() {
        let payload = b"every format ".repeat(300);
        let chunks = || ok_stream(payload.chunks(89).map(view).collect());

        // Every format reaches the adapters through `Format`, so this needs no per-format arm and
        // cannot fall out of step when a format is added.
        for &format in Format::ALL {
            let memory = GlobalPool::new();

            let encoded = collect(Compress::new(chunks(), format.encoder().build(memory.clone()))).expect("compression succeeds");
            let plain = collect(Decompress::new(ok_stream(vec![encoded]), format.decoder().build(memory))).expect("decompression succeeds");

            assert_eq!(plain.to_vec(), payload, "{format:?} failed to round trip");
        }
    }

    #[test]
    fn compresses_an_empty_source() {
        let source = ok_stream(Vec::new());
        let gzip = collect(Compress::new(source, gzip::Encoder::new(GlobalPool::new()))).expect("compression succeeds");

        assert_eq!(gzip.range(0..2).to_vec(), vec![0x1f, 0x8b]);
    }

    #[test]
    fn decompresses_a_byte_at_a_time() {
        let memory = GlobalPool::new();
        let encoded = crate::gzip::compress(view(b"one byte at a time"), memory.clone()).expect("compression succeeds");
        let single_bytes = (0..encoded.len()).map(|i| encoded.range(i..=i)).collect();

        let plain = collect(Decompress::new(ok_stream(single_bytes), gzip::Decoder::new(memory))).expect("decompression succeeds");

        assert_eq!(plain.to_vec(), b"one byte at a time".to_vec());
    }

    #[test]
    fn reports_a_failing_source_as_a_source_error() {
        let failing = stream::iter(vec![Err(std::io::Error::other("transport died"))]);

        let error = collect(Compress::new(failing, gzip::Encoder::new(GlobalPool::new()))).expect_err("the source failure surfaces");

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
        let mut stream = Box::pin(Decompress::new(source, gzip::Decoder::new(GlobalPool::new())));

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
        let mut stream = Box::pin(Decompress::new(ok_stream(vec![gzip]), gzip::Decoder::new(memory)));

        block_on(async {
            while stream.next().await.is_some() {}
            assert!(stream.next().await.is_none(), "a completed stream stays ended");
        });
    }

    #[test]
    fn reports_corrupt_input_from_the_decompress_adapter() {
        let source = ok_stream(vec![view(b"this is not gzip")]);

        let error = collect(Decompress::new(source, gzip::Decoder::new(GlobalPool::new()))).expect_err("bad data is rejected");

        assert!(error.is_corrupt_data(), "got {error}");
    }

    #[test]
    fn honours_a_pre_configured_decoder() {
        let memory = GlobalPool::new();
        let gzip = crate::gzip::compress(view(&vec![0_u8; 4 * 1024 * 1024]), memory.clone()).expect("compression succeeds");

        let decoder = gzip::Decoder::builder()
            .limits(DecompressionLimits::new().with_max_output_len(1024))
            .build(memory);

        let error = collect(Decompress::new(ok_stream(vec![gzip]), decoder)).expect_err("the cap fires");

        assert!(error.is_limit_exceeded(), "got {error}");
    }

    #[test]
    fn honours_a_pre_configured_encoder() {
        let memory = GlobalPool::new();
        let payload = b"the quick brown fox ".repeat(400);

        let encoder = gzip::Encoder::builder().level(Level::BEST).build(memory.clone());
        let gzip = collect(Compress::new(ok_stream(vec![view(&payload)]), encoder)).expect("compression succeeds");

        let plain = collect(Decompress::new(ok_stream(vec![gzip]), gzip::Decoder::new(memory))).expect("decompression succeeds");

        assert_eq!(plain.to_vec(), payload);
    }

    #[test]
    fn tolerates_empty_chunks_from_the_source() {
        let memory = GlobalPool::new();
        let source = ok_stream(vec![BytesView::new(), view(b"data"), BytesView::new()]);

        let gzip = collect(Compress::new(source, gzip::Encoder::new(memory.clone()))).expect("compression succeeds");
        let plain = collect(Decompress::new(ok_stream(vec![gzip]), gzip::Decoder::new(memory))).expect("decompression succeeds");

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

        let gzip = collect(Compress::new(source, gzip::Encoder::new(GlobalPool::new()))).expect("compression succeeds");

        assert_eq!(gzip.range(0..2).to_vec(), vec![0x1f, 0x8b]);
    }

    #[test]
    fn streams_are_send_so_they_can_cross_task_boundaries() {
        // `!Send` is infectious: a stream that cannot move between tasks is unusable in most async
        // runtimes. The `Send` supertrait on `Encoder`/`Decoder` is what makes the boxed codec Send.
        fn assert_send<T: Send>(_: &T) {}

        let memory = GlobalPool::new();
        assert_send(&Compress::new(ok_stream(Vec::new()), gzip::Encoder::new(memory.clone())));
        assert_send(&Decompress::new(ok_stream(Vec::new()), gzip::Decoder::new(memory)));
    }

    #[test]
    fn debug_is_available_for_diagnostics() {
        let empty = stream::iter(Vec::<std::result::Result<BytesView, std::io::Error>>::new());
        let stream = Compress::new(empty, gzip::Encoder::new(GlobalPool::new()));

        assert!(format!("{stream:?}").contains("Compress"));
    }
}
