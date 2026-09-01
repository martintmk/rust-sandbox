// Licensed under the MIT License.

//! One contract, applied to every format.
//!
//! These tests exist to keep the abstraction honest. Every format goes through the same scenarios,
//! so a format that behaves differently from its siblings — or an abstraction that quietly only
//! fits the deflate family — fails here rather than surprising a consumer.

#![cfg(any(feature = "brotli", feature = "deflate", feature = "gzip", feature = "zlib", feature = "zstd"))]

use std::num::{NonZeroU32, NonZeroUsize};

use bytesbuf::mem::GlobalPool;
use bytesbuf::{BytesBuf, BytesView};
use compressed::format::Format;
use compressed::{Compress, Compression, Decompress, DecompressionLimits, Level, Output, Pool};

fn view(bytes: &[u8]) -> BytesView {
    BytesView::copied_from_slice(bytes, &GlobalPool::new())
}

/// Builds a view split into `segment` sized spans, exercising the multi-segment paths.
fn fragmented(bytes: &[u8], segment: usize) -> BytesView {
    let memory = GlobalPool::new();
    BytesView::from_views(bytes.chunks(segment).map(|chunk| BytesView::copied_from_slice(chunk, &memory)))
}

fn chunk(size: usize) -> NonZeroUsize {
    NonZeroUsize::new(size).expect("test chunk sizes are never zero")
}

/// Drives any compression operation to completion, feeding the input in `feed` sized pieces.
fn process<D>(compression: &mut dyn Compression<Mode = D>, input: &BytesView, feed: usize) -> compressed::Result<BytesView> {
    let mut offset = 0;
    let mut collected = BytesBuf::new();

    loop {
        match compression.pull()? {
            Output::Data(data) => collected.put_bytes(data),
            Output::Done => break,
            Output::NeedInput => {
                if offset >= input.len() {
                    compression.finish();
                    continue;
                }

                let end = (offset + feed).min(input.len());
                compression.push(input.range(offset..end))?;
                offset = end;
            }
        }
    }

    Ok(collected.consume_all())
}

fn compress(compressor: &mut dyn Compression<Mode = Compress>, input: &BytesView, feed: usize) -> compressed::Result<BytesView> {
    process(compressor, input, feed)
}

fn decompress(decompressor: &mut dyn Compression<Mode = Decompress>, input: &BytesView, feed: usize) -> compressed::Result<BytesView> {
    process(decompressor, input, feed)
}

/// Generates the shared contract for one format, using its concrete module so the builders are
/// exercised too, not just the runtime `Format` factories.
macro_rules! format_contract {
    ($module:ident, $format:expr) => {
        mod $module {
            use compressed::$module;

            use super::*;

            const FORMAT: Format = $format;

            fn payload() -> Vec<u8> {
                b"the quick brown fox jumps over the lazy dog; pack my box with five dozen liquor jugs. ".repeat(300)
            }

            #[test]
            fn round_trips_a_payload() {
                let memory = GlobalPool::new();
                let data = payload();

                let compressed = $module::compress(view(&data), memory.clone()).expect("compression succeeds");
                assert!(compressed.len() < data.len(), "the payload should compress");

                let plain = $module::decompress(compressed, memory).expect("decompression succeeds");
                assert_eq!(plain.to_vec(), data);
            }

            #[test]
            fn compress_matches_driving_the_operation_by_hand() {
                // The convenience must be exactly the manual loop, not an approximation of it.
                let memory = GlobalPool::new();
                let data = payload();

                let convenient = $module::Compressor::new(memory.clone())
                    .compress(view(&data))
                    .expect("compression succeeds");

                let mut by_hand = $module::Compressor::new(memory.clone());
                by_hand.push(view(&data)).expect("push succeeds");
                Compression::finish(&mut by_hand);
                let mut collected = BytesBuf::new();
                while let Some(chunk) = Compression::pull(&mut by_hand).expect("pull succeeds").into_data() {
                    collected.put_bytes(chunk);
                }

                assert_eq!(convenient.to_vec(), collected.consume_all().to_vec());

                let plain = $module::Decompressor::new(memory)
                    .decompress(convenient)
                    .expect("decompression succeeds");

                assert_eq!(plain.to_vec(), data);
            }

            #[test]
            fn compress_and_decompress_work_through_a_trait_object() {
                // Provided methods are easy to break for `dyn`, so reach them that way too.
                let memory = GlobalPool::new();
                let data = payload();

                let compressor: Box<dyn Compression<Mode = Compress>> = Box::new($module::Compressor::new(memory.clone()));
                let compressed = compressor.compress(view(&data)).expect("compression succeeds");

                let decompressor: Box<dyn Compression<Mode = Decompress>> = Box::new($module::Decompressor::new(memory));

                assert_eq!(
                    decompressor.decompress(compressed).expect("decompression succeeds").to_vec(),
                    data
                );
            }

            #[test]
            fn round_trips_empty_input() {
                let memory = GlobalPool::new();

                let compressed = $module::compress(BytesView::new(), memory.clone()).expect("compression succeeds");
                let plain = $module::decompress(compressed, memory).expect("decompression succeeds");

                assert!(plain.is_empty());
            }

            #[test]
            fn round_trips_a_multi_segment_view() {
                // The reason this crate exists: input arrives as a chain of spans, never as one
                // contiguous slice.
                for (segment, repeats) in [(1_usize, 40_usize), (7, 200), (1024, 2_000)] {
                    let data = b"multi segment ".repeat(repeats);
                    let memory = GlobalPool::new();

                    let compressed = $module::compress(fragmented(&data, segment), memory.clone()).expect("compression succeeds");
                    let plain = $module::decompress(compressed, memory).expect("decompression succeeds");

                    assert_eq!(plain.to_vec(), data, "failed at {segment} byte segments");
                }
            }

            #[test]
            fn round_trips_when_driven_one_byte_at_a_time() {
                // Worst case for a push/pull codec: minimal input pieces and minimal output chunks.
                let memory = GlobalPool::new();
                let data = b"drip fed".repeat(20);

                let mut compressor = $module::Compressor::builder()
                    .output_chunk_size(chunk(1))
                    .build(memory.clone());
                let compressed = compress(&mut compressor, &view(&data), 1).expect("compression succeeds");

                let mut decompressor = $module::Decompressor::builder().output_chunk_size(chunk(1)).build(memory);
                let plain = decompress(&mut decompressor, &compressed, 1).expect("decompression succeeds");

                assert_eq!(plain.to_vec(), data);
            }

            #[test]
            fn honours_the_output_chunk_size() {
                let memory = GlobalPool::new();
                let data = payload();

                let mut compressor = $module::Compressor::builder()
                    .output_chunk_size(chunk(256))
                    .build(memory.clone());
                compressor.push(view(&data)).expect("push succeeds");
                Compression::finish(&mut compressor);

                let mut compressed = BytesBuf::new();
                while let Some(piece) = Compression::pull(&mut compressor).expect("pull succeeds").into_data() {
                    assert!(piece.len() <= 256, "chunk of {} bytes exceeded the bound", piece.len());
                    compressed.put_bytes(piece);
                }

                let mut decompressor = $module::Decompressor::builder().output_chunk_size(chunk(256)).build(memory);
                decompressor.push(compressed.consume_all()).expect("push succeeds");
                Compression::finish(&mut decompressor);

                let mut plain = BytesBuf::new();
                while let Some(piece) = Compression::pull(&mut decompressor).expect("pull succeeds").into_data() {
                    assert!(piece.len() <= 256, "chunk of {} bytes exceeded the bound", piece.len());
                    plain.put_bytes(piece);
                }

                assert_eq!(plain.consume_all().to_vec(), data);
            }

            #[test]
            fn every_level_produces_a_decodable_stream() {
                let data = payload();

                for raw in 0..=Level::MAX.get() {
                    let level = Level::new(raw).expect("level is in range");
                    let memory = GlobalPool::new();

                    let mut compressor = $module::Compressor::builder().level(level).build(memory.clone());
                    let compressed = compress(&mut compressor, &view(&data), usize::MAX).expect("compression succeeds");

                    let plain = $module::decompress(compressed, memory).expect("decompression succeeds");
                    assert_eq!(plain.to_vec(), data, "level {raw} did not round trip");
                }
            }

            #[test]
            fn tracks_byte_counts() {
                let memory = GlobalPool::new();
                let data = payload();

                let mut compressor = $module::Compressor::new(memory.clone());
                let compressed = compress(&mut compressor, &view(&data), usize::MAX).expect("compression succeeds");

                assert_eq!(compressor.total_in(), data.len() as u64);
                assert_eq!(compressor.total_out(), compressed.len() as u64);

                let mut decompressor = $module::Decompressor::new(memory);
                let plain = decompress(&mut decompressor, &compressed, usize::MAX).expect("decompression succeeds");

                assert_eq!(decompressor.total_in(), compressed.len() as u64);
                assert_eq!(decompressor.total_out(), plain.len() as u64);
            }

            #[test]
            fn rejects_a_truncated_stream() {
                let memory = GlobalPool::new();
                let compressed = $module::compress(view(&payload()), memory).expect("compression succeeds");

                for cut in [1, compressed.len() / 3, compressed.len() - 1] {
                    let error = $module::decompress(compressed.range(0..cut), GlobalPool::new())
                        .expect_err("a truncated stream must not decompress successfully");

                    assert!(
                        error.is_unexpected_end_of_stream() || error.is_corrupt_data(),
                        "truncating at {cut} gave an unexpected classification: {error}"
                    );
                }
            }

            #[test]
            fn rejects_input_after_finish() {
                let mut compressor = $module::Compressor::new(GlobalPool::new());
                Compression::finish(&mut compressor);

                let error = compressor.push(view(b"late")).expect_err("push after finish is rejected");
                assert!(error.is_invalid_state());

                let mut decompressor = $module::Decompressor::new(GlobalPool::new());
                Compression::finish(&mut decompressor);

                let error = decompressor.push(view(b"late")).expect_err("push after finish is rejected");
                assert!(error.is_invalid_state());
            }

            #[test]
            fn asks_for_more_input_before_finish() {
                let mut compressor = $module::Compressor::new(GlobalPool::new());
                compressor.push(view(b"partial")).expect("push succeeds");

                let output = loop {
                    match Compression::pull(&mut compressor).expect("pull succeeds") {
                        Output::Data(_) => {}
                        other => break other,
                    }
                };

                assert!(output.is_need_input(), "an unfinished compressor must ask for more input");
            }

            #[test]
            fn enforces_a_configured_expansion_limit() {
                // A ratio the data is guaranteed to exceed, so the mechanism itself is tested
                // rather than whichever default the format happens to carry.
                let memory = GlobalPool::new();
                let bomb = $module::compress(view(&vec![0_u8; 16 * 1024 * 1024]), memory.clone()).expect("compression succeeds");

                let mut decompressor = $module::Decompressor::builder()
                    .limits(DecompressionLimits::new().with_max_ratio(NonZeroU32::new(4).expect("4 is not zero")))
                    .build(memory);
                decompressor.push(bomb).expect("push succeeds");
                Compression::finish(&mut decompressor);

                let error = loop {
                    match Compression::pull(&mut decompressor) {
                        Ok(Output::Data(_)) => {}
                        Ok(_) => panic!("the bomb decompressed fully instead of being rejected"),
                        Err(error) => break error,
                    }
                };

                assert!(error.is_limit_exceeded(), "got {error}");
                assert!(
                    decompressor.total_out() < 16 * 1024 * 1024,
                    "the guard should fire before the full expansion"
                );
            }

            #[test]
            fn default_limits_accept_ordinary_highly_compressible_data() {
                // Regression guard. A single portable ratio limit was calibrated on deflate, whose
                // structural ceiling is ~1032x. Brotli legitimately reaches tens of thousands of
                // times expansion, so that limit rejected ordinary repetitive input — a repeated
                // sentence, and JSON. Each format now carries its own default.
                let memory = GlobalPool::new();

                let cases: [(&str, Vec<u8>); 3] = [
                    ("repeated short string", b"windowed ".repeat(20_000)),
                    (
                        "repeated sentence",
                        b"the quick brown fox jumps over the lazy dog. ".repeat(20_000),
                    ),
                    (
                        "repetitive json",
                        br#"{"id":1,"name":"widget","tags":["a","b"]},"#.repeat(12_000),
                    ),
                ];

                for (label, data) in cases {
                    let compressed = $module::compress(view(&data), memory.clone()).expect("compression succeeds");
                    let ratio = data.len() / compressed.len().max(1);

                    let plain = $module::decompress(compressed, memory.clone())
                        .unwrap_or_else(|error| panic!("default limits rejected {label} at {ratio}x expansion: {error}"));

                    assert_eq!(plain.to_vec(), data, "{label} did not round trip");
                }
            }

            #[test]
            fn an_absolute_cap_is_enforced() {
                let memory = GlobalPool::new();
                let compressed = $module::compress(view(&vec![0_u8; 4 * 1024 * 1024]), memory.clone()).expect("compression succeeds");

                let mut decompressor = $module::Decompressor::builder()
                    .limits(DecompressionLimits::new().without_max_ratio().with_max_output_len(1024))
                    .build(memory);
                decompressor.push(compressed).expect("push succeeds");
                Compression::finish(&mut decompressor);

                let error = loop {
                    match Compression::pull(&mut decompressor) {
                        Ok(Output::Data(_)) => {}
                        Ok(_) => panic!("the cap should have fired"),
                        Err(error) => break error,
                    }
                };

                assert!(error.is_limit_exceeded(), "got {error}");
            }

            #[test]
            fn trusted_callers_can_opt_out_of_the_limits() {
                let memory = GlobalPool::new();
                let data = vec![0_u8; 4 * 1024 * 1024];
                let compressed = $module::compress(view(&data), memory.clone()).expect("compression succeeds");

                let mut decompressor = $module::Decompressor::builder()
                    .limits(DecompressionLimits::UNLIMITED)
                    .build(memory);
                let plain = decompress(&mut decompressor, &compressed, usize::MAX).expect("decompression succeeds");

                assert_eq!(plain.len(), data.len());
            }

            #[test]
            fn corruption_is_detected_or_changes_the_output() {
                // Formats with a checksum report corruption; raw deflate has none, so the honest
                // universal guarantee is only that corrupt input does not silently reproduce the
                // original bytes.
                let memory = GlobalPool::new();
                let data = payload();
                let compressed = $module::compress(view(&data), memory).expect("compression succeeds");
                let original = compressed.to_vec();

                for index in [0, original.len() / 2, original.len() - 1] {
                    let mut corrupted = original.clone();
                    corrupted[index] ^= 0xff;

                    match $module::decompress(view(&corrupted), GlobalPool::new()) {
                        Ok(plain) => assert_ne!(plain.to_vec(), data, "corruption at {index} went unnoticed"),
                        Err(error) => assert!(
                            error.is_corrupt_data() || error.is_unexpected_end_of_stream() || error.is_limit_exceeded(),
                            "corruption at {index} gave an unexpected classification: {error}"
                        ),
                    }
                }
            }

            #[test]
            fn the_runtime_factory_matches_the_module() {
                // `Format` must produce codecs equivalent to the concrete modules, or runtime
                // selection would silently behave differently from compile-time selection.
                let memory = GlobalPool::new();
                let data = payload();

                let via_module = $module::compress(view(&data), memory.clone()).expect("compression succeeds");
                let via_format = FORMAT.compress(view(&data), memory.clone()).expect("compression succeeds");

                assert_eq!(
                    via_module.to_vec(),
                    via_format.to_vec(),
                    "runtime and compile-time selection diverged"
                );

                // Either output must decompress through either path.
                assert_eq!(
                    FORMAT
                        .decompress(via_module, memory.clone())
                        .expect("decompression succeeds")
                        .to_vec(),
                    data
                );
                assert_eq!(
                    $module::decompress(via_format, memory)
                        .expect("decompression succeeds")
                        .to_vec(),
                    data
                );
            }

            #[test]
            fn works_through_boxed_trait_objects() {
                let memory = GlobalPool::new();
                let data = payload();

                let mut compressor = FORMAT.compressor().build(memory.clone());
                let compressed = compress(&mut *compressor, &view(&data), usize::MAX).expect("compression succeeds");

                let mut decompressor = FORMAT.decompressor().build(memory);
                let plain = decompress(&mut *decompressor, &compressed, usize::MAX).expect("decompression succeeds");

                assert_eq!(plain.to_vec(), data);
            }

            #[test]
            fn pooling_does_not_change_the_output() {
                // Reuse is an optimisation, so it must change nothing a caller can observe. The
                // baseline and the pooled runs share one input view on purpose: some engines
                // legitimately vary with input segmentation (zstd records the content size in its
                // frame header only when the whole input arrives in one call), so a fresh view per
                // run would compare allocator behaviour rather than pooling.
                let pool = Pool::new();
                let input = view(&payload());
                let baseline = {
                    let mut compressor = $module::Compressor::builder()
                        .output_chunk_size(chunk(4096))
                        .build(GlobalPool::new());
                    compress(&mut compressor, &input, usize::MAX).expect("compression succeeds")
                };

                // Several rounds: the first compressor always misses the pool, so only later rounds
                // exercise a recycled engine.
                for round in 0..5 {
                    let mut compressor = $module::Compressor::builder()
                        .output_chunk_size(chunk(4096))
                        .pool(pool.clone())
                        .build(GlobalPool::new());
                    let pooled = compress(&mut compressor, &input, usize::MAX).expect("compression succeeds");
                    drop(compressor);

                    assert_eq!(pooled.to_vec(), baseline.to_vec(), "round {round}: pooled output diverged");

                    let mut decompressor = $module::Decompressor::builder().pool(pool.clone()).build(GlobalPool::new());
                    let plain = decompress(&mut decompressor, &pooled, usize::MAX).expect("decompression succeeds");

                    assert_eq!(plain.to_vec(), payload(), "round {round}: pooled decompressor lost data");
                }
            }

            #[test]
            fn an_engine_abandoned_mid_stream_is_cleaned_before_reuse() {
                // A request cancelled part-way through returns a half-used engine.
                let pool = Pool::new();
                let input = view(&payload());
                let baseline = {
                    let mut compressor = $module::Compressor::builder()
                        .output_chunk_size(chunk(4096))
                        .build(GlobalPool::new());
                    compress(&mut compressor, &input, usize::MAX).expect("compression succeeds")
                };

                for round in 0..4 {
                    {
                        let mut abandoned = $module::Compressor::builder()
                            .output_chunk_size(chunk(4096))
                            .pool(pool.clone())
                            .build(GlobalPool::new());
                        abandoned.push(input.clone()).expect("push succeeds");
                        let _ = Compression::pull(&mut abandoned).expect("pull succeeds");
                        // Dropped without finishing, so its engine is mid-frame.
                    }

                    let mut compressor = $module::Compressor::builder()
                        .output_chunk_size(chunk(4096))
                        .pool(pool.clone())
                        .build(GlobalPool::new());
                    let recovered = compress(&mut compressor, &input, usize::MAX).expect("compression succeeds");

                    assert_eq!(recovered.to_vec(), baseline.to_vec(), "round {round}: a dirty engine leaked");
                }
            }

            #[test]
            fn an_engine_left_dirty_by_a_failed_decompression_is_cleaned_before_reuse() {
                let pool = Pool::new();
                let compressed = $module::compress(view(&payload()), GlobalPool::new()).expect("compression succeeds");
                let garbage = view(&b"definitely not a valid stream".repeat(20));

                for round in 0..4 {
                    {
                        let mut failing = $module::Decompressor::builder().pool(pool.clone()).build(GlobalPool::new());
                        let _ = decompress(&mut failing, &garbage, usize::MAX);
                    }

                    let mut decompressor = $module::Decompressor::builder().pool(pool.clone()).build(GlobalPool::new());
                    let plain = decompress(&mut decompressor, &compressed, usize::MAX).expect("a clean stream still decompresses");

                    assert_eq!(
                        plain.to_vec(),
                        payload(),
                        "round {round}: a failed decompress poisoned the pool"
                    );
                }
            }

            #[test]
            fn levels_never_share_engines() {
                // Resetting a compressor preserves its level, so engines must be keyed by it.
                let pool = Pool::new();
                let input = view(&payload());
                let levels = [Level::NONE, Level::FAST, Level::DEFAULT, Level::BEST];

                let baselines: Vec<_> = levels
                    .iter()
                    .map(|&level| {
                        let mut compressor = $module::Compressor::builder()
                            .level(level)
                            .output_chunk_size(chunk(4096))
                            .build(GlobalPool::new());
                        compress(&mut compressor, &input, usize::MAX)
                            .expect("compression succeeds")
                            .to_vec()
                    })
                    .collect();

                for round in 0..4 {
                    for (index, &level) in levels.iter().enumerate() {
                        let mut compressor = $module::Compressor::builder()
                            .level(level)
                            .output_chunk_size(chunk(4096))
                            .pool(pool.clone())
                            .build(GlobalPool::new());
                        let pooled = compress(&mut compressor, &input, usize::MAX).expect("compression succeeds");

                        assert_eq!(
                            pooled.to_vec(),
                            baselines[index],
                            "round {round}: a pooled engine came back at the wrong level"
                        );
                    }
                }
            }

            #[test]
            fn two_live_codecs_get_distinct_engines() {
                // All three compressors are driven by exactly the same sequence, so any difference in
                // their output is the engine and nothing else.
                fn run(compressor: &mut $module::Compressor, input: &BytesView) -> Vec<u8> {
                    compressor.push(input.clone()).expect("push succeeds");
                    Compression::finish(compressor);

                    let mut collected = BytesBuf::new();
                    while let Some(chunk) = Compression::pull(compressor).expect("pull succeeds").into_data() {
                        collected.put_bytes(chunk);
                    }

                    collected.consume_all().to_vec()
                }

                fn build(pool: Option<&Pool>) -> $module::Compressor {
                    let builder = $module::Compressor::builder().output_chunk_size(chunk(4096));
                    match pool {
                        Some(pool) => builder.pool(pool.clone()).build(GlobalPool::new()),
                        None => builder.build(GlobalPool::new()),
                    }
                }

                let pool = Pool::new();
                let input = view(&payload());
                let baseline = run(&mut build(None), &input);

                // Prime the pool so there is exactly one idle engine for two codecs to want.
                drop(run(&mut build(Some(&pool)), &input));

                let mut first = build(Some(&pool));
                let mut second = build(Some(&pool));

                // Interleave: both are live before either finishes, so they cannot be sharing.
                first.push(input.clone()).expect("push succeeds");
                second.push(input.clone()).expect("push succeeds");
                Compression::finish(&mut first);
                Compression::finish(&mut second);

                for (label, compressor) in [("first", &mut first), ("second", &mut second)] {
                    let mut collected = BytesBuf::new();
                    while let Some(chunk) = Compression::pull(compressor).expect("pull succeeds").into_data() {
                        collected.put_bytes(chunk);
                    }

                    assert_eq!(
                        collected.consume_all().to_vec(),
                        baseline,
                        "{label} compressor was corrupted by sharing"
                    );
                }
            }

            #[test]
            fn a_codec_outliving_its_pool_handle_still_works() {
                let input = view(&payload());
                let baseline = {
                    let mut compressor = $module::Compressor::builder()
                        .output_chunk_size(chunk(4096))
                        .build(GlobalPool::new());
                    compress(&mut compressor, &input, usize::MAX).expect("compression succeeds")
                };

                let mut compressor = {
                    let pool = Pool::new();
                    let compressor = $module::Compressor::builder()
                        .output_chunk_size(chunk(4096))
                        .pool(pool.clone())
                        .build(GlobalPool::new());
                    drop(pool);
                    compressor
                };

                let pooled = compress(&mut compressor, &input, usize::MAX).expect("compression succeeds");

                assert_eq!(
                    pooled.to_vec(),
                    baseline.to_vec(),
                    "dropping the pool handle changed the output"
                );
            }

            #[test]
            fn pool_capacity_bounds_retention_without_changing_output() {
                let input = view(&payload());
                let baseline = {
                    let mut compressor = $module::Compressor::builder()
                        .output_chunk_size(chunk(4096))
                        .build(GlobalPool::new());
                    compress(&mut compressor, &input, usize::MAX).expect("compression succeeds")
                };

                for capacity in [0_usize, 1, 4] {
                    let pool = Pool::with_capacity(capacity);
                    assert_eq!(pool.capacity(), capacity);

                    for round in 0..12 {
                        let mut compressor = $module::Compressor::builder()
                            .output_chunk_size(chunk(4096))
                            .pool(pool.clone())
                            .build(GlobalPool::new());
                        let pooled = compress(&mut compressor, &input, usize::MAX).expect("compression succeeds");

                        assert_eq!(
                            pooled.to_vec(),
                            baseline.to_vec(),
                            "capacity {capacity} round {round}: output changed"
                        );
                    }
                }
            }

            #[test]
            fn empty_input_round_trips_through_a_pool() {
                let pool = Pool::new();
                let baseline = {
                    let mut compressor = $module::Compressor::builder()
                        .output_chunk_size(chunk(4096))
                        .build(GlobalPool::new());
                    compress(&mut compressor, &BytesView::new(), usize::MAX).expect("compression succeeds")
                };

                for round in 0..4 {
                    let mut compressor = $module::Compressor::builder()
                        .output_chunk_size(chunk(4096))
                        .pool(pool.clone())
                        .build(GlobalPool::new());
                    let pooled = compress(&mut compressor, &BytesView::new(), usize::MAX).expect("compression succeeds");
                    drop(compressor);

                    assert_eq!(pooled.to_vec(), baseline.to_vec(), "round {round}: empty framing changed");

                    let mut decompressor = $module::Decompressor::builder().pool(pool.clone()).build(GlobalPool::new());
                    let plain = decompress(&mut decompressor, &pooled, usize::MAX).expect("decompression succeeds");

                    assert!(plain.is_empty(), "round {round}: empty input produced bytes");
                }
            }

            #[test]
            fn truncation_is_still_detected_when_pooled() {
                let pool = Pool::new();
                let compressed = $module::compress(view(&payload()), GlobalPool::new()).expect("compression succeeds");

                for round in 0..3 {
                    // A healthy decompress first, so the next decompressor is guaranteed to be recycled.
                    let mut healthy = $module::Decompressor::builder().pool(pool.clone()).build(GlobalPool::new());
                    decompress(&mut healthy, &compressed, usize::MAX).expect("the full stream decompresses");
                    drop(healthy);

                    let mut decompressor = $module::Decompressor::builder().pool(pool.clone()).build(GlobalPool::new());
                    let error = decompress(&mut decompressor, &compressed.range(0..compressed.len() - 1), usize::MAX)
                        .expect_err("a truncated stream must not decompress successfully");

                    assert!(
                        error.is_unexpected_end_of_stream() || error.is_corrupt_data(),
                        "round {round}: unexpected classification {error}"
                    );
                }
            }

            #[test]
            fn works_through_generic_format_agnostic_code() {
                /// Code written once, against the traits, with no knowledge of the format.
                fn transcode(
                    mut compressor: impl Compression<Mode = Compress>,
                    mut decompressor: impl Compression<Mode = Decompress>,
                    data: &[u8],
                ) -> Vec<u8> {
                    let compressed = compress(&mut compressor, &view(data), 64).expect("compression succeeds");
                    decompress(&mut decompressor, &compressed, 64)
                        .expect("decompression succeeds")
                        .to_vec()
                }

                let memory = GlobalPool::new();
                let data = payload();

                assert_eq!(
                    transcode(
                        $module::Compressor::new(memory.clone()),
                        $module::Decompressor::new(memory),
                        &data
                    ),
                    data
                );
            }
        }
    };
}

#[cfg(feature = "deflate")]
format_contract!(deflate, Format::Deflate);
#[cfg(feature = "zlib")]
format_contract!(zlib, Format::Zlib);
#[cfg(feature = "gzip")]
format_contract!(gzip, Format::Gzip);
#[cfg(feature = "brotli")]
format_contract!(brotli, Format::Brotli);
#[cfg(feature = "zstd")]
format_contract!(zstd, Format::Zstd);

#[test]
fn every_compiled_format_satisfies_the_contract() {
    // Guards against a format being added to `Format::ALL` without being added to the suite above.
    let covered = usize::from(cfg!(feature = "deflate"))
        + usize::from(cfg!(feature = "zlib"))
        + usize::from(cfg!(feature = "gzip"))
        + usize::from(cfg!(feature = "brotli"))
        + usize::from(cfg!(feature = "zstd"));

    assert_eq!(
        Format::ALL.len(),
        covered,
        "a format was added without extending the contract suite"
    );
}

#[test]
fn formats_produce_mutually_incompatible_streams() {
    // Each format must be genuinely distinct: decoding one format's output with another's decompressor
    // must fail rather than silently produce garbage.
    let memory = GlobalPool::new();
    let data = b"cross format check ".repeat(200);

    for &produced_by in Format::ALL {
        let compressed = produced_by.compress(view(&data), memory.clone()).expect("compression succeeds");

        for &decompressed_by in Format::ALL {
            if produced_by == decompressed_by {
                continue;
            }

            if let Ok(plain) = decompressed_by.decompress(compressed.clone(), memory.clone()) {
                assert_ne!(
                    plain.to_vec(),
                    data,
                    "{decompressed_by:?} decompressed a {produced_by:?} stream as if it were its own"
                );
            }
        }
    }
}

#[test]
fn a_decompressor_can_be_chosen_from_a_declared_encoding() {
    // The end-to-end runtime scenario: a peer declares its encoding in a header, and the decompressor is
    // chosen from that string.
    let memory = GlobalPool::new();
    let data = b"declared encoding ".repeat(100);

    for &format in Format::ALL {
        let Some(token) = format.content_encoding() else {
            continue;
        };

        let compressed = format.compress(view(&data), memory.clone()).expect("compression succeeds");

        let declared = Format::from_content_encoding(token).expect("the token is supported");
        let plain = declared.decompress(compressed, memory.clone()).expect("decompression succeeds");

        assert_eq!(plain.to_vec(), data, "{format:?} did not decompress via its declared token");
    }
}

#[test]
fn content_negotiation_selects_a_supported_format() {
    // The other end-to-end runtime scenario: pick an encoding from what a client says it accepts.
    fn negotiate(header: &str) -> Option<Format> {
        Format::from_accept_encoding(header).next()
    }

    assert_eq!(negotiate("identity"), None);

    #[cfg(feature = "gzip")]
    assert_eq!(negotiate("identity, gzip"), Some(Format::Gzip));
    #[cfg(feature = "zlib")]
    assert_eq!(negotiate("deflate"), Some(Format::Zlib));

    // Negotiation must degrade gracefully: a token for a format this build does not support is
    // skipped in favour of the next one the client offered.
    #[cfg(feature = "brotli")]
    assert_eq!(negotiate("br, gzip, deflate"), Some(Format::Brotli));
    #[cfg(all(not(feature = "brotli"), feature = "gzip"))]
    assert_eq!(negotiate("br, gzip, deflate"), Some(Format::Gzip));
    #[cfg(all(not(feature = "brotli"), not(feature = "gzip"), feature = "zlib"))]
    assert_eq!(negotiate("br, gzip, deflate"), Some(Format::Zlib));
    #[cfg(all(not(feature = "brotli"), not(feature = "gzip"), not(feature = "zlib")))]
    assert_eq!(negotiate("br, gzip, deflate"), None, "raw deflate has no HTTP token");

    // Whatever was negotiated must actually work.
    if let Some(format) = negotiate("br, gzip, deflate") {
        let memory = GlobalPool::new();
        let compressed = format.compress(view(b"negotiated"), memory.clone()).expect("compression succeeds");

        assert_eq!(
            format.decompress(compressed, memory).expect("decompression succeeds").to_vec(),
            b"negotiated".to_vec()
        );
    }
}

/// Format-specific settings: how a format extends the shared builder without breaking the contract.
#[cfg(feature = "brotli")]
mod format_specific_settings {
    use compressed::brotli;
    use compressed::brotli::{Mode, WindowSize};

    use super::*;

    #[test]
    fn a_format_specific_setting_still_produces_a_conforming_stream() {
        // Whatever brotli-only knobs are set, the result must still satisfy the shared contract.
        let memory = GlobalPool::new();
        let data = b"format specific settings ".repeat(400);

        let mut tuned = brotli::Compressor::builder()
            .level(Level::BEST)
            .mode(Mode::Text)
            .window_size(WindowSize::new(20).expect("20 is in range"))
            .build(memory.clone());

        let compressed = compress(&mut tuned, &view(&data), usize::MAX).expect("compression succeeds");
        let plain = brotli::decompress(compressed, memory).expect("decompression succeeds");

        assert_eq!(plain.to_vec(), data);
    }

    #[test]
    fn window_size_rejects_values_outside_brotlis_range() {
        // Configuration input must report a mistake, not panic.
        assert_eq!(WindowSize::new(9), None);
        assert_eq!(WindowSize::new(25), None);
        assert_eq!(WindowSize::new(10), Some(WindowSize::MIN));
        assert_eq!(WindowSize::new(24), Some(WindowSize::MAX));
        assert_eq!(WindowSize::default(), WindowSize::DEFAULT);
    }

    #[test]
    fn a_smaller_window_still_round_trips() {
        let memory = GlobalPool::new();
        let data = b"windowed ".repeat(20_000);

        for exponent in [10, 16, 24] {
            let window = WindowSize::new(exponent).expect("exponent is in range");
            let mut tuned = brotli::Compressor::builder().window_size(window).build(memory.clone());

            let compressed = compress(&mut tuned, &view(&data), usize::MAX).expect("compression succeeds");
            let plain = brotli::decompress(compressed, memory.clone()).expect("decompression succeeds");

            assert_eq!(plain.to_vec(), data, "window 2^{exponent} did not round trip");
        }
    }

    #[test]
    fn a_runtime_chosen_format_can_still_reach_format_specific_settings() {
        // The documented escape hatch: a runtime `Format` builder cannot carry a brotli-only
        // setting, so branch on the format, use the concrete builder, and box the result. That
        // works because a boxed compression operation is itself a `Compression`.
        fn compressor_for(format: Format, memory: GlobalPool) -> Box<dyn Compression<Mode = Compress>> {
            match format {
                Format::Brotli => Box::new(brotli::Compressor::builder().mode(Mode::Text).build(memory)),
                other => other.compressor().build(memory),
            }
        }

        let memory = GlobalPool::new();
        let data = b"escape hatch ".repeat(200);

        for &format in Format::ALL {
            let mut tuned = compressor_for(format, memory.clone());
            let compressed = compress(&mut *tuned, &view(&data), usize::MAX).expect("compression succeeds");

            let plain = format.decompress(compressed, memory.clone()).expect("decompression succeeds");
            assert_eq!(plain.to_vec(), data, "{format:?} failed through the escape hatch");
        }
    }

    #[test]
    fn text_mode_does_not_change_the_decompressed_bytes() {
        // The mode is a compressor-side hint only: it must never alter what comes back out.
        let memory = GlobalPool::new();
        let data = b"the quick brown fox jumps over the lazy dog ".repeat(300);

        for mode in [Mode::Generic, Mode::Text, Mode::Font] {
            let mut tuned = brotli::Compressor::builder().mode(mode).build(memory.clone());
            let compressed = compress(&mut tuned, &view(&data), usize::MAX).expect("compression succeeds");

            let plain = brotli::decompress(compressed, memory.clone()).expect("decompression succeeds");
            assert_eq!(plain.to_vec(), data, "{mode:?} changed the decompressed bytes");
        }
    }
}

/// Engine reuse must be invisible: a recycled compressor has to behave exactly like a fresh one.
#[cfg(feature = "gzip")]
mod pooling {
    use compressed::gzip;

    use super::*;

    fn compress_with(pool: Option<Pool>, level: Level, data: &[u8]) -> BytesView {
        let memory = GlobalPool::new();
        let builder = gzip::Compressor::builder().level(level);
        let builder = match pool {
            Some(pool) => builder.pool(pool),
            None => builder,
        };

        let mut compressor = builder.build(memory);
        compress(&mut compressor, &view(data), usize::MAX).expect("compression succeeds")
    }

    #[test]
    fn a_recycled_engine_produces_byte_identical_output() {
        // The whole safety argument for pooling: reset state must leave no trace of the previous
        // stream. Compare many pooled rounds against a fresh-engine baseline.
        let pool = Pool::new();
        let payloads = [
            b"first request body".repeat(50),
            b"a completely different second body, longer".repeat(80),
            b"third".repeat(500),
        ];

        for round in 0..4 {
            for payload in &payloads {
                let pooled = compress_with(Some(pool.clone()), Level::DEFAULT, payload);
                let fresh = compress_with(None, Level::DEFAULT, payload);

                assert_eq!(
                    pooled.to_vec(),
                    fresh.to_vec(),
                    "round {round}: pooled output diverged from a fresh engine"
                );
                assert_eq!(gzip::decompress(pooled, GlobalPool::new()).expect("decompress").to_vec(), *payload);
            }
        }
    }

    #[test]
    fn a_compressor_abandoned_mid_stream_does_not_poison_the_pool() {
        // A request cancelled part-way through returns a dirty engine. The next user must still
        // get a clean stream.
        let pool = Pool::new();

        {
            let mut abandoned = gzip::Compressor::builder().pool(pool.clone()).build(GlobalPool::new());
            abandoned.push(view(&b"half a stream ".repeat(100))).expect("push succeeds");
            let _ = Compression::pull(&mut abandoned).expect("pull succeeds");
            // Dropped without `finish`, so its engine is mid-stream.
        }

        let recovered = compress_with(Some(pool), Level::DEFAULT, b"a fresh stream");
        let fresh = compress_with(None, Level::DEFAULT, b"a fresh stream");

        assert_eq!(recovered.to_vec(), fresh.to_vec(), "a recycled dirty engine must be reset");
        assert_eq!(
            gzip::decompress(recovered, GlobalPool::new()).expect("decompress").to_vec(),
            b"a fresh stream".to_vec()
        );
    }

    #[test]
    fn levels_do_not_share_engines() {
        // Reset preserves the level, so a level-9 request must never receive a level-1 engine.
        let pool = Pool::new();
        let payload = b"the quick brown fox jumps over the lazy dog ".repeat(200);

        let fast = compress_with(Some(pool.clone()), Level::FAST, &payload);
        let best = compress_with(Some(pool), Level::BEST, &payload);

        assert_eq!(fast.to_vec(), compress_with(None, Level::FAST, &payload).to_vec());
        assert_eq!(best.to_vec(), compress_with(None, Level::BEST, &payload).to_vec());
        assert!(best.len() <= fast.len(), "level 9 must still out-compress level 1");
    }

    #[test]
    fn a_pool_is_shared_across_threads() {
        // The point of the design: one handle lives in a client and is cloned per request.
        let pool = Pool::new();
        let payload = b"concurrent body ".repeat(200);

        std::thread::scope(|scope| {
            for _ in 0..8 {
                let pool = pool.clone();
                let payload = payload.clone();
                scope.spawn(move || {
                    for _ in 0..10 {
                        let compressed = compress_with(Some(pool.clone()), Level::DEFAULT, &payload);
                        assert_eq!(
                            gzip::decompress(compressed, GlobalPool::new()).expect("decompress").to_vec(),
                            payload
                        );
                    }
                });
            }
        });
    }

    #[test]
    fn a_pooled_decompressor_round_trips_every_format() {
        // Whether or not a format's engine is actually recycled is an implementation detail; the
        // decompressed bytes must be identical either way.
        let payloads = [b"first response body".repeat(60), b"a different second body".repeat(90)];

        for &format in Format::ALL {
            let pool = Pool::new();
            let memory = GlobalPool::new();

            for round in 0..4 {
                for payload in &payloads {
                    let compressed = format.compress(view(payload), memory.clone()).expect("compression succeeds");

                    let mut decompressor = format.decompressor().pool(pool.clone()).build(memory.clone());
                    let plain = decompress(&mut *decompressor, &compressed, usize::MAX).expect("decompression succeeds");

                    assert_eq!(plain.to_vec(), *payload, "{format:?} round {round} diverged when pooled");
                }
            }
        }
    }

    #[cfg(feature = "zlib")]
    #[test]
    fn a_decompressor_abandoned_mid_stream_does_not_poison_the_pool() {
        use compressed::zlib;

        let pool = Pool::new();
        let memory = GlobalPool::new();
        let payload = b"a stream that gets cut short ".repeat(200);
        let compressed = zlib::compress(view(&payload), memory.clone()).expect("compression succeeds");

        {
            let mut abandoned = zlib::Decompressor::builder().pool(pool.clone()).build(memory.clone());
            abandoned.push(compressed.range(0..compressed.len() / 2)).expect("push succeeds");
            let _ = Compression::pull(&mut abandoned).expect("pull succeeds");
            // Dropped mid-stream, so its engine is dirty.
        }

        let mut recovered = zlib::Decompressor::builder().pool(pool).build(memory);
        let plain = decompress(&mut recovered, &compressed, usize::MAX).expect("decompression succeeds");

        assert_eq!(plain.to_vec(), payload, "a recycled dirty decompressor must be reset");
    }

    #[test]
    fn gzip_decompressors_are_not_recycled() {
        // `Decompress::reset` takes a boolean that cannot express gzip framing, so a recycled gzip
        // decompressor would silently decompress as raw deflate. It must therefore never be pooled —
        // and the caller must not be able to tell the difference.
        let pool = Pool::new();
        let memory = GlobalPool::new();
        let payload = b"gzip stays correct ".repeat(200);
        let compressed = gzip::compress(view(&payload), memory.clone()).expect("compression succeeds");

        for round in 0..5 {
            let mut decompressor = gzip::Decompressor::builder().pool(pool.clone()).build(memory.clone());
            let plain = decompress(&mut decompressor, &compressed, usize::MAX).expect("decompression succeeds");

            assert_eq!(plain.to_vec(), payload, "gzip round {round} decompressed incorrectly");
        }
    }

    #[test]
    fn a_zero_capacity_pool_still_works() {
        let pool = Pool::with_capacity(0);
        let payload = b"no recycling here".repeat(20);

        let compressed = compress_with(Some(pool), Level::DEFAULT, &payload);

        assert_eq!(
            gzip::decompress(compressed, GlobalPool::new()).expect("decompress").to_vec(),
            payload
        );
    }
}

/// The riskiest pooling bug: deflate, zlib and gzip share one engine type, so a mis-keyed pool
/// would hand a zlib compressor to a gzip request and emit a well-formed stream in the wrong
/// format. Nothing else in the suite would catch that.
#[test]
fn formats_never_share_pooled_engines() {
    let pool = Pool::new();
    let data = b"interleaved through one pool ".repeat(200);
    let input = view(&data);

    let baselines: Vec<_> = Format::ALL
        .iter()
        .map(|&format| {
            let mut compressor = format.compressor().output_chunk_size(chunk(4096)).build(GlobalPool::new());
            let bytes = compress(&mut *compressor, &input, usize::MAX)
                .expect("compression succeeds")
                .to_vec();
            (format, bytes)
        })
        .collect();

    // Interleave, so every format has had a turn before any is asked again.
    for round in 0..6 {
        for (format, baseline) in &baselines {
            let mut compressor = format
                .compressor()
                .output_chunk_size(chunk(4096))
                .pool(pool.clone())
                .build(GlobalPool::new());
            let pooled = compress(&mut *compressor, &input, usize::MAX).expect("compression succeeds");
            drop(compressor);

            assert_eq!(
                &pooled.to_vec(),
                baseline,
                "{format:?} round {round}: interleaving formats through one pool changed the output"
            );

            // And the bytes really are this format's, not a sibling's that happens to decompress.
            for (other, _) in &baselines {
                let mut reader = other.decompressor().pool(pool.clone()).build(GlobalPool::new());
                let decompressed = decompress(&mut *reader, &pooled, usize::MAX);

                if other == format {
                    assert_eq!(
                        decompressed.expect("its own decompressor must accept it").to_vec(),
                        data,
                        "{format:?} round {round}: own decompressor failed"
                    );
                } else if let Ok(plain) = decompressed {
                    assert_ne!(plain.to_vec(), data, "{other:?} decompressed a {format:?} stream as its own");
                }
            }
        }
    }
}

/// One pool shared by many threads, the way a client would actually use it.
#[test]
fn a_shared_pool_is_correct_under_concurrency() {
    let pool = Pool::new();
    let data = b"concurrent request body ".repeat(150);

    let baselines: Vec<_> = Format::ALL
        .iter()
        .map(|&format| {
            let input = view(&data);
            let mut compressor = format.compressor().output_chunk_size(chunk(4096)).build(GlobalPool::new());
            let bytes = compress(&mut *compressor, &input, usize::MAX)
                .expect("compression succeeds")
                .to_vec();
            (format, bytes)
        })
        .collect();

    std::thread::scope(|scope| {
        for _ in 0..8 {
            let pool = pool.clone();
            let data = data.clone();
            let baselines = baselines.clone();

            scope.spawn(move || {
                // Each thread builds its own view, so segmentation is stable within the thread.
                let input = view(&data);

                for round in 0..10 {
                    for (format, baseline) in &baselines {
                        let mut compressor = format
                            .compressor()
                            .output_chunk_size(chunk(4096))
                            .pool(pool.clone())
                            .build(GlobalPool::new());
                        let pooled = compress(&mut *compressor, &input, usize::MAX).expect("compression succeeds");
                        drop(compressor);

                        assert_eq!(&pooled.to_vec(), baseline, "{format:?} round {round}: concurrent pooling diverged");

                        let mut decompressor = format.decompressor().pool(pool.clone()).build(GlobalPool::new());
                        let plain = decompress(&mut *decompressor, &pooled, usize::MAX).expect("decompression succeeds");

                        assert_eq!(plain.to_vec(), data, "{format:?} round {round}: concurrent decompress lost data");
                    }
                }
            });
        }
    });
}

/// A long run must not drift: the hundredth message has to match the first.
#[test]
fn pooled_output_does_not_drift_over_many_reuses() {
    let pool = Pool::new();
    let data = b"steady state ".repeat(120);

    for &format in Format::ALL {
        let input = view(&data);
        let mut first: Option<Vec<u8>> = None;

        for round in 0..60 {
            let mut compressor = format
                .compressor()
                .output_chunk_size(chunk(4096))
                .pool(pool.clone())
                .build(GlobalPool::new());
            let pooled = compress(&mut *compressor, &input, usize::MAX)
                .expect("compression succeeds")
                .to_vec();
            drop(compressor);

            match first {
                None => first = Some(pooled),
                Some(ref expected) => assert_eq!(&pooled, expected, "{format:?} round {round}: output drifted"),
            }
        }
    }
}
