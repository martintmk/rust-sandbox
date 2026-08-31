// Licensed under the MIT License.

//! One contract, applied to every format.
//!
//! These tests exist to keep the abstraction honest. Every format goes through the same scenarios,
//! so a format that behaves differently from its siblings — or an abstraction that quietly only
//! fits the deflate family — fails here rather than surprising a consumer.

#![cfg(any(feature = "brotli", feature = "deflate", feature = "gzip", feature = "zlib"))]

use std::num::{NonZeroU32, NonZeroUsize};

use bytesbuf::BytesView;
use bytesbuf::mem::GlobalPool;
use compressed::{Decoder, DecompressionLimits, Encoder, Format, Level, Output};

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

/// Drives any encoder to completion, feeding the input in `feed` sized pieces.
fn encode(encoder: &mut dyn Encoder, input: &BytesView, feed: usize) -> compressed::Result<BytesView> {
    let mut offset = 0;
    let mut parts = Vec::new();

    loop {
        match encoder.pull()? {
            Output::Data(data) => parts.push(data),
            Output::Done => break,
            Output::NeedInput => {
                if offset >= input.len() {
                    encoder.finish();
                    continue;
                }

                let end = (offset + feed).min(input.len());
                encoder.push(input.range(offset..end))?;
                offset = end;
            }
        }
    }

    Ok(BytesView::from_views(parts))
}

/// Drives any decoder to completion, feeding the input in `feed` sized pieces.
fn decode(decoder: &mut dyn Decoder, input: &BytesView, feed: usize) -> compressed::Result<BytesView> {
    let mut offset = 0;
    let mut parts = Vec::new();

    loop {
        match decoder.pull()? {
            Output::Data(data) => parts.push(data),
            Output::Done => break,
            Output::NeedInput => {
                if offset >= input.len() {
                    decoder.finish();
                    continue;
                }

                let end = (offset + feed).min(input.len());
                decoder.push(input.range(offset..end))?;
                offset = end;
            }
        }
    }

    Ok(BytesView::from_views(parts))
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

                let encoded = $module::compress(view(&data), memory.clone()).expect("compression succeeds");
                assert!(encoded.len() < data.len(), "the payload should compress");

                let plain = $module::decompress(encoded, memory).expect("decompression succeeds");
                assert_eq!(plain.to_vec(), data);
            }

            #[test]
            fn round_trips_empty_input() {
                let memory = GlobalPool::new();

                let encoded = $module::compress(BytesView::new(), memory.clone()).expect("compression succeeds");
                let plain = $module::decompress(encoded, memory).expect("decompression succeeds");

                assert!(plain.is_empty());
            }

            #[test]
            fn round_trips_a_multi_segment_view() {
                // The reason this crate exists: input arrives as a chain of spans, never as one
                // contiguous slice.
                for (segment, repeats) in [(1_usize, 40_usize), (7, 200), (1024, 2_000)] {
                    let data = b"multi segment ".repeat(repeats);
                    let memory = GlobalPool::new();

                    let encoded = $module::compress(fragmented(&data, segment), memory.clone()).expect("compression succeeds");
                    let plain = $module::decompress(encoded, memory).expect("decompression succeeds");

                    assert_eq!(plain.to_vec(), data, "failed at {segment} byte segments");
                }
            }

            #[test]
            fn round_trips_when_driven_one_byte_at_a_time() {
                // Worst case for a push/pull codec: minimal input pieces and minimal output chunks.
                let memory = GlobalPool::new();
                let data = b"drip fed".repeat(20);

                let mut encoder = $module::Encoder::builder().output_chunk_size(chunk(1)).build(memory.clone());
                let encoded = encode(&mut encoder, &view(&data), 1).expect("compression succeeds");

                let mut decoder = $module::Decoder::builder().output_chunk_size(chunk(1)).build(memory);
                let plain = decode(&mut decoder, &encoded, 1).expect("decompression succeeds");

                assert_eq!(plain.to_vec(), data);
            }

            #[test]
            fn honours_the_output_chunk_size() {
                let memory = GlobalPool::new();
                let data = payload();

                let mut encoder = $module::Encoder::builder()
                    .output_chunk_size(chunk(256))
                    .build(memory.clone());
                encoder.push(view(&data)).expect("push succeeds");
                Encoder::finish(&mut encoder);

                let mut encoded = Vec::new();
                while let Some(piece) = Encoder::pull(&mut encoder).expect("pull succeeds").into_data() {
                    assert!(piece.len() <= 256, "chunk of {} bytes exceeded the bound", piece.len());
                    encoded.push(piece);
                }

                let mut decoder = $module::Decoder::builder().output_chunk_size(chunk(256)).build(memory);
                decoder.push(BytesView::from_views(encoded)).expect("push succeeds");
                Decoder::finish(&mut decoder);

                let mut plain = Vec::new();
                while let Some(piece) = Decoder::pull(&mut decoder).expect("pull succeeds").into_data() {
                    assert!(piece.len() <= 256, "chunk of {} bytes exceeded the bound", piece.len());
                    plain.push(piece);
                }

                assert_eq!(BytesView::from_views(plain).to_vec(), data);
            }

            #[test]
            fn every_level_produces_a_decodable_stream() {
                let data = payload();

                for raw in 0..=Level::MAX {
                    let level = Level::new(raw).expect("level is in range");
                    let memory = GlobalPool::new();

                    let mut encoder = $module::Encoder::builder().level(level).build(memory.clone());
                    let encoded = encode(&mut encoder, &view(&data), usize::MAX).expect("compression succeeds");

                    let plain = $module::decompress(encoded, memory).expect("decompression succeeds");
                    assert_eq!(plain.to_vec(), data, "level {raw} did not round trip");
                }
            }

            #[test]
            fn tracks_byte_counts() {
                let memory = GlobalPool::new();
                let data = payload();

                let mut encoder = $module::Encoder::new(memory.clone());
                let encoded = encode(&mut encoder, &view(&data), usize::MAX).expect("compression succeeds");

                assert_eq!(encoder.total_in(), data.len() as u64);
                assert_eq!(encoder.total_out(), encoded.len() as u64);

                let mut decoder = $module::Decoder::new(memory);
                let plain = decode(&mut decoder, &encoded, usize::MAX).expect("decompression succeeds");

                assert_eq!(decoder.total_in(), encoded.len() as u64);
                assert_eq!(decoder.total_out(), plain.len() as u64);
            }

            #[test]
            fn rejects_a_truncated_stream() {
                let memory = GlobalPool::new();
                let encoded = $module::compress(view(&payload()), memory).expect("compression succeeds");

                for cut in [1, encoded.len() / 3, encoded.len() - 1] {
                    let error = $module::decompress(encoded.range(0..cut), GlobalPool::new())
                        .expect_err("a truncated stream must not decode successfully");

                    assert!(
                        error.is_unexpected_end_of_stream() || error.is_corrupt_data(),
                        "truncating at {cut} gave an unexpected classification: {error}"
                    );
                }
            }

            #[test]
            fn rejects_input_after_finish() {
                let mut encoder = $module::Encoder::new(GlobalPool::new());
                Encoder::finish(&mut encoder);

                let error = encoder.push(view(b"late")).expect_err("push after finish is rejected");
                assert!(error.is_invalid_state());

                let mut decoder = $module::Decoder::new(GlobalPool::new());
                Decoder::finish(&mut decoder);

                let error = decoder.push(view(b"late")).expect_err("push after finish is rejected");
                assert!(error.is_invalid_state());
            }

            #[test]
            fn asks_for_more_input_before_finish() {
                let mut encoder = $module::Encoder::new(GlobalPool::new());
                encoder.push(view(b"partial")).expect("push succeeds");

                let output = loop {
                    match Encoder::pull(&mut encoder).expect("pull succeeds") {
                        Output::Data(_) => {}
                        other => break other,
                    }
                };

                assert!(output.is_need_input(), "an unfinished encoder must ask for more input");
            }

            #[test]
            fn enforces_a_configured_expansion_limit() {
                // A ratio the data is guaranteed to exceed, so the mechanism itself is tested
                // rather than whichever default the format happens to carry.
                let memory = GlobalPool::new();
                let bomb = $module::compress(view(&vec![0_u8; 16 * 1024 * 1024]), memory.clone()).expect("compression succeeds");

                let mut decoder = $module::Decoder::builder()
                    .limits(DecompressionLimits::new().with_max_ratio(NonZeroU32::new(4).expect("4 is not zero")))
                    .build(memory);
                decoder.push(bomb).expect("push succeeds");
                Decoder::finish(&mut decoder);

                let error = loop {
                    match Decoder::pull(&mut decoder) {
                        Ok(Output::Data(_)) => {}
                        Ok(_) => panic!("the bomb decoded fully instead of being rejected"),
                        Err(error) => break error,
                    }
                };

                assert!(error.is_limit_exceeded(), "got {error}");
                assert!(
                    decoder.total_out() < 16 * 1024 * 1024,
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
                    let encoded = $module::compress(view(&data), memory.clone()).expect("compression succeeds");
                    let ratio = data.len() / encoded.len().max(1);

                    let plain = $module::decompress(encoded, memory.clone())
                        .unwrap_or_else(|error| panic!("default limits rejected {label} at {ratio}x expansion: {error}"));

                    assert_eq!(plain.to_vec(), data, "{label} did not round trip");
                }
            }

            #[test]
            fn an_absolute_cap_is_enforced() {
                let memory = GlobalPool::new();
                let encoded = $module::compress(view(&vec![0_u8; 4 * 1024 * 1024]), memory.clone()).expect("compression succeeds");

                let mut decoder = $module::Decoder::builder()
                    .limits(DecompressionLimits::new().without_max_ratio().with_max_output_len(1024))
                    .build(memory);
                decoder.push(encoded).expect("push succeeds");
                Decoder::finish(&mut decoder);

                let error = loop {
                    match Decoder::pull(&mut decoder) {
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
                let encoded = $module::compress(view(&data), memory.clone()).expect("compression succeeds");

                let mut decoder = $module::Decoder::builder()
                    .limits(DecompressionLimits::UNLIMITED)
                    .build(memory);
                let plain = decode(&mut decoder, &encoded, usize::MAX).expect("decompression succeeds");

                assert_eq!(plain.len(), data.len());
            }

            #[test]
            fn corruption_is_detected_or_changes_the_output() {
                // Formats with a checksum report corruption; raw deflate has none, so the honest
                // universal guarantee is only that corrupt input does not silently reproduce the
                // original bytes.
                let memory = GlobalPool::new();
                let data = payload();
                let encoded = $module::compress(view(&data), memory).expect("compression succeeds");
                let original = encoded.to_vec();

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

                // Either output must decode through either path.
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

                let mut encoder = FORMAT.encoder().build(memory.clone());
                let encoded = encode(&mut *encoder, &view(&data), usize::MAX).expect("compression succeeds");

                let mut decoder = FORMAT.decoder().build(memory);
                let plain = decode(&mut *decoder, &encoded, usize::MAX).expect("decompression succeeds");

                assert_eq!(plain.to_vec(), data);
            }

            #[test]
            fn works_through_generic_format_agnostic_code() {
                /// Code written once, against the traits, with no knowledge of the format.
                fn transcode(mut encoder: impl Encoder, mut decoder: impl Decoder, data: &[u8]) -> Vec<u8> {
                    let encoded = encode(&mut encoder, &view(data), 64).expect("compression succeeds");
                    decode(&mut decoder, &encoded, 64).expect("decompression succeeds").to_vec()
                }

                let memory = GlobalPool::new();
                let data = payload();

                assert_eq!(
                    transcode($module::Encoder::new(memory.clone()), $module::Decoder::new(memory), &data),
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

#[test]
fn every_compiled_format_satisfies_the_contract() {
    // Guards against a format being added to `Format::ALL` without being added to the suite above.
    let covered = usize::from(cfg!(feature = "deflate"))
        + usize::from(cfg!(feature = "zlib"))
        + usize::from(cfg!(feature = "gzip"))
        + usize::from(cfg!(feature = "brotli"));

    assert_eq!(
        Format::ALL.len(),
        covered,
        "a format was added without extending the contract suite"
    );
}

#[test]
fn formats_produce_mutually_incompatible_streams() {
    // Each format must be genuinely distinct: decoding one format's output with another's decoder
    // must fail rather than silently produce garbage.
    let memory = GlobalPool::new();
    let data = b"cross format check ".repeat(200);

    for &produced_by in Format::ALL {
        let encoded = produced_by.compress(view(&data), memory.clone()).expect("compression succeeds");

        for &decoded_by in Format::ALL {
            if produced_by == decoded_by {
                continue;
            }

            if let Ok(plain) = decoded_by.decompress(encoded.clone(), memory.clone()) {
                assert_ne!(
                    plain.to_vec(),
                    data,
                    "{decoded_by:?} decoded a {produced_by:?} stream as if it were its own"
                );
            }
        }
    }
}

#[test]
fn a_decoder_can_be_chosen_from_a_declared_encoding() {
    // The end-to-end runtime scenario: a peer declares its encoding in a header, and the decoder is
    // chosen from that string.
    let memory = GlobalPool::new();
    let data = b"declared encoding ".repeat(100);

    for &format in Format::ALL {
        let Some(token) = format.content_encoding() else {
            continue;
        };

        let encoded = format.compress(view(&data), memory.clone()).expect("compression succeeds");

        let declared = Format::from_content_encoding(token).expect("the token is supported");
        let plain = declared.decompress(encoded, memory.clone()).expect("decompression succeeds");

        assert_eq!(plain.to_vec(), data, "{format:?} did not decode via its declared token");
    }
}

#[test]
fn content_negotiation_selects_a_supported_format() {
    // The other end-to-end runtime scenario: pick an encoding from what a client says it accepts.
    fn negotiate(accept_encoding: &str) -> Option<Format> {
        accept_encoding.split(',').find_map(Format::from_content_encoding)
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
        let encoded = format.compress(view(b"negotiated"), memory.clone()).expect("compression succeeds");

        assert_eq!(
            format.decompress(encoded, memory).expect("decompression succeeds").to_vec(),
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

        let mut tuned = brotli::Encoder::builder()
            .level(Level::BEST)
            .mode(Mode::Text)
            .window_size(WindowSize::new(20).expect("20 is in range"))
            .build(memory.clone());

        let encoded = encode(&mut tuned, &view(&data), usize::MAX).expect("compression succeeds");
        let plain = brotli::decompress(encoded, memory).expect("decompression succeeds");

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
            let mut tuned = brotli::Encoder::builder().window_size(window).build(memory.clone());

            let encoded = encode(&mut tuned, &view(&data), usize::MAX).expect("compression succeeds");
            let plain = brotli::decompress(encoded, memory.clone()).expect("decompression succeeds");

            assert_eq!(plain.to_vec(), data, "window 2^{exponent} did not round trip");
        }
    }

    #[test]
    fn a_runtime_chosen_format_can_still_reach_format_specific_settings() {
        // The documented escape hatch: a runtime `Format` builder cannot carry a brotli-only
        // setting, so branch on the format, use the concrete builder, and box the result. That
        // works because `Box<dyn Encoder>` is itself an `Encoder`.
        fn encoder_for(format: Format, memory: GlobalPool) -> Box<dyn Encoder> {
            match format {
                Format::Brotli => Box::new(brotli::Encoder::builder().mode(Mode::Text).build(memory)),
                other => other.encoder().build(memory),
            }
        }

        let memory = GlobalPool::new();
        let data = b"escape hatch ".repeat(200);

        for &format in Format::ALL {
            let mut tuned = encoder_for(format, memory.clone());
            let encoded = encode(&mut *tuned, &view(&data), usize::MAX).expect("compression succeeds");

            let plain = format.decompress(encoded, memory.clone()).expect("decompression succeeds");
            assert_eq!(plain.to_vec(), data, "{format:?} failed through the escape hatch");
        }
    }

    #[test]
    fn text_mode_does_not_change_the_decoded_bytes() {
        // The mode is an encoder-side hint only: it must never alter what comes back out.
        let memory = GlobalPool::new();
        let data = b"the quick brown fox jumps over the lazy dog ".repeat(300);

        for mode in [Mode::Generic, Mode::Text, Mode::Font] {
            let mut tuned = brotli::Encoder::builder().mode(mode).build(memory.clone());
            let encoded = encode(&mut tuned, &view(&data), usize::MAX).expect("compression succeeds");

            let plain = brotli::decompress(encoded, memory.clone()).expect("decompression succeeds");
            assert_eq!(plain.to_vec(), data, "{mode:?} changed the decoded bytes");
        }
    }
}
