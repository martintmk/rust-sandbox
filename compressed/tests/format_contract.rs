// Licensed under the MIT License.

//! One contract, applied to every format.
//!
//! These tests exist to keep the abstraction honest. Every format goes through the same scenarios,
//! so a format that behaves differently from its siblings — or an abstraction that quietly only
//! fits the deflate family — fails here rather than surprising a consumer.

use std::num::NonZeroUsize;

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
            fn enforces_the_expansion_limit() {
                // Highly compressible input expands far past the default ratio.
                let memory = GlobalPool::new();
                let bomb = $module::compress(view(&vec![0_u8; 16 * 1024 * 1024]), memory.clone()).expect("compression succeeds");

                let mut decoder = $module::Decoder::new(memory);
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

format_contract!(deflate, Format::Deflate);
format_contract!(zlib, Format::Zlib);
format_contract!(gzip, Format::Gzip);
#[cfg(feature = "brotli")]
format_contract!(brotli, Format::Brotli);

#[test]
fn every_compiled_format_satisfies_the_contract() {
    // Guards against a format being added to `Format::ALL` without being added to the suite above.
    let covered = if cfg!(feature = "brotli") { 4 } else { 3 };

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

    assert_eq!(negotiate("identity, gzip"), Some(Format::Gzip));
    assert_eq!(negotiate("deflate"), Some(Format::Zlib));
    assert_eq!(negotiate("identity"), None);

    // Negotiation must degrade gracefully: a token for a format this build does not support is
    // skipped in favour of the next one the client offered.
    #[cfg(feature = "brotli")]
    assert_eq!(negotiate("br, gzip, deflate"), Some(Format::Brotli));
    #[cfg(not(feature = "brotli"))]
    assert_eq!(negotiate("br, gzip, deflate"), Some(Format::Gzip));

    let memory = GlobalPool::new();
    let format = negotiate("gzip").expect("gzip is always supported");
    let encoded = format.compress(view(b"negotiated"), memory.clone()).expect("compression succeeds");

    assert_eq!(
        format.decompress(encoded, memory).expect("decompression succeeds").to_vec(),
        b"negotiated".to_vec()
    );
}
