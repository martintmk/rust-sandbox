// Licensed under the MIT License.

//! Throughput and allocation behaviour of the codecs.
//!
//! Every benchmark reports both time and allocations, because this crate's central claims are about
//! allocation: input is consumed segment by segment without being flattened, output is written into
//! a caller-supplied memory provider, and [`Pool`] recycles engine state. Timings alone would not
//! show a regression in any of those.
//!
//! Allocation figures come from [`alloc_tracker`], which installs a global allocator for this
//! binary and prints a per-iteration table when the session is dropped.
//!
//! Read the zstd rows with care. `zstd` allocates its compression and decompression contexts
//! through its own allocator rather than Rust's, so those allocations are invisible here and the
//! zstd rows understate the true cost. Its timings are unaffected, so compare zstd against itself
//! on time and against the other formats only on the figures the global allocator can see.

use std::hint::black_box;
use std::num::NonZeroUsize;
use std::time::Instant;

use alloc_tracker::{Allocator, Operation, Session};
use bytesbuf::BytesView;
use bytesbuf::mem::GlobalPool;
use compressed::brotli::{self, WindowSize};
use compressed::{Format, Level, Pool};
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

#[global_allocator]
static ALLOCATOR: Allocator<std::alloc::System> = Allocator::system();

/// Sizes chosen to bracket real traffic: a small API response, a page, and a large document.
const SIZES: [usize; 3] = [1024, 64 * 1024, 1024 * 1024];

/// Builds a payload that compresses like real data rather than like a repeated string.
///
/// A repeated token collapses to a handful of bytes at every level, which hides the differences
/// between formats and between levels.
fn payload(size: usize) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(size + 128);
    let mut seed = 0x2545_f491_4f6c_dd1d_u64;

    let mut id = 0_u64;
    while bytes.len() < size {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;

        bytes.extend_from_slice(
            format!(
                r#"{{"id":{id},"user":"user_{}","score":{},"tag":"{}","ok":{}}},"#,
                seed % 100_000,
                seed % 1_000,
                ["alpha", "beta", "gamma", "delta", "epsilon"][(seed % 5) as usize],
                seed % 2 == 0
            )
            .as_bytes(),
        );
        id += 1;
    }

    bytes.truncate(size);
    bytes
}

fn view(bytes: &[u8], memory: &GlobalPool) -> BytesView {
    BytesView::copied_from_slice(bytes, memory)
}

/// Splits a payload into `segment` sized spans, the shape this crate exists to handle.
fn fragmented(bytes: &[u8], segment: usize, memory: &GlobalPool) -> BytesView {
    BytesView::from_views(bytes.chunks(segment).map(|chunk| BytesView::copied_from_slice(chunk, memory)))
}

fn chunk(size: usize) -> NonZeroUsize {
    NonZeroUsize::new(size).expect("benchmark chunk sizes are never zero")
}

/// Compresses a view, returning the output so the optimiser cannot discard the work.
fn compress(format: Format, pool: Option<&Pool>, chunk_size: Option<NonZeroUsize>, input: &BytesView, memory: &GlobalPool) -> BytesView {
    let builder = format.encoder();
    let builder = match chunk_size {
        Some(size) => builder.output_chunk_size(size),
        None => builder,
    };
    let builder = match pool {
        Some(pool) => builder.pool(pool.clone()),
        None => builder,
    };

    let mut encoder = builder.build(memory.clone());
    encoder.push(input.clone()).expect("push succeeds");
    encoder.finish();

    let mut parts = Vec::new();
    while let Some(data) = encoder.pull().expect("pull succeeds").into_data() {
        parts.push(data);
    }

    BytesView::from_views(parts)
}

fn decompress(format: Format, pool: Option<&Pool>, input: &BytesView, memory: &GlobalPool) -> BytesView {
    let builder = format.decoder();
    let builder = match pool {
        Some(pool) => builder.pool(pool.clone()),
        None => builder,
    };

    let mut decoder = builder.build(memory.clone());
    decoder.push(input.clone()).expect("push succeeds");
    decoder.finish();

    let mut parts = Vec::new();
    while let Some(data) = decoder.pull().expect("pull succeeds").into_data() {
        parts.push(data);
    }

    BytesView::from_views(parts)
}

/// Runs `body` under Criterion while attributing its allocations to `operation`.
fn measured(bencher: &mut criterion::Bencher<'_>, operation: &Operation, mut body: impl FnMut()) {
    bencher.iter_custom(|iterations| {
        let start = Instant::now();
        let _span = operation.measure_process().iterations(iterations);

        for _ in 0..iterations {
            body();
        }

        start.elapsed()
    });
}

fn compression(criterion: &mut Criterion, session: &Session) {
    let mut group = criterion.benchmark_group("compress");

    for size in SIZES {
        let bytes = payload(size);
        group.throughput(Throughput::Bytes(size as u64));

        for &format in Format::ALL {
            let memory = GlobalPool::new();
            let input = view(&bytes, &memory);
            let name = format!("{format:?}/{size}");
            let operation = session.operation(&format!("compress {name}"));

            group.bench_function(BenchmarkId::from_parameter(&name), |bencher| {
                measured(bencher, &operation, || {
                    black_box(compress(format, None, None, &input, &memory));
                });
            });
        }
    }

    group.finish();
}

fn decompression(criterion: &mut Criterion, session: &Session) {
    let mut group = criterion.benchmark_group("decompress");

    for size in SIZES {
        let bytes = payload(size);
        group.throughput(Throughput::Bytes(size as u64));

        for &format in Format::ALL {
            let memory = GlobalPool::new();
            let encoded = compress(format, None, None, &view(&bytes, &memory), &memory);
            let name = format!("{format:?}/{size}");
            let operation = session.operation(&format!("decompress {name}"));

            group.bench_function(BenchmarkId::from_parameter(&name), |bencher| {
                measured(bencher, &operation, || {
                    black_box(decompress(format, None, &encoded, &memory));
                });
            });
        }
    }

    group.finish();
}

/// The headline claim for [`Pool`]: recycling engine state removes per-message setup.
///
/// Also the regression guard for it. If pooled stops beating unpooled, or stops allocating less,
/// something has broken.
fn pooling(criterion: &mut Criterion, session: &Session) {
    let mut group = criterion.benchmark_group("pooling");
    let bytes = payload(4096);
    group.throughput(Throughput::Bytes(bytes.len() as u64));

    for &format in Format::ALL {
        let memory = GlobalPool::new();
        let input = view(&bytes, &memory);
        let pool = Pool::new();
        let encoded = compress(format, None, None, &input, &memory);

        // Warm the pool so the measured iterations all hit it.
        drop(compress(format, Some(&pool), None, &input, &memory));
        drop(decompress(format, Some(&pool), &encoded, &memory));

        for (label, pooled) in [("fresh", None), ("pooled", Some(&pool))] {
            let name = format!("{format:?}/compress/{label}");
            let operation = session.operation(&format!("pool {name}"));

            group.bench_function(BenchmarkId::from_parameter(&name), |bencher| {
                measured(bencher, &operation, || {
                    black_box(compress(format, pooled, None, &input, &memory));
                });
            });

            let name = format!("{format:?}/decompress/{label}");
            let operation = session.operation(&format!("pool {name}"));

            group.bench_function(BenchmarkId::from_parameter(&name), |bencher| {
                measured(bencher, &operation, || {
                    black_box(decompress(format, pooled, &encoded, &memory));
                });
            });
        }
    }

    group.finish();
}

/// Input arrives as a chain of spans, so the cost of that chain is the crate's reason to exist.
///
/// A regression here — for instance flattening the view before handing it to the engine — would
/// show up as a jump in allocations for the fragmented cases.
fn segmentation(criterion: &mut Criterion, session: &Session) {
    let mut group = criterion.benchmark_group("segmentation");
    let bytes = payload(64 * 1024);
    group.throughput(Throughput::Bytes(bytes.len() as u64));

    let format = *Format::ALL.first().expect("at least one format is compiled in");
    let memory = GlobalPool::new();

    for segment in [64_usize, 1024, 16 * 1024] {
        let input = fragmented(&bytes, segment, &memory);
        let name = format!("{segment}B spans");
        let operation = session.operation(&format!("segment {name}"));

        group.bench_function(BenchmarkId::from_parameter(&name), |bencher| {
            measured(bencher, &operation, || {
                black_box(compress(format, None, None, &input, &memory));
            });
        });
    }

    let contiguous = view(&bytes, &memory);
    let operation = session.operation("segment contiguous");
    group.bench_function(BenchmarkId::from_parameter("contiguous"), |bencher| {
        measured(bencher, &operation, || {
            black_box(compress(format, None, None, &contiguous, &memory));
        });
    });

    group.finish();
}

/// The output chunk size trades per-call overhead against buffer churn.
///
/// The engines zero-fill the uninitialized output slice they are handed, so a larger chunk is not
/// automatically better; this is what settles the default.
fn chunk_size(criterion: &mut Criterion, session: &Session) {
    let mut group = criterion.benchmark_group("chunk_size");
    let bytes = payload(256 * 1024);
    group.throughput(Throughput::Bytes(bytes.len() as u64));

    let format = *Format::ALL.first().expect("at least one format is compiled in");
    let memory = GlobalPool::new();
    let input = view(&bytes, &memory);

    for size in [1024_usize, 8 * 1024, 64 * 1024, 512 * 1024] {
        let name = format!("{size}B chunks");
        let operation = session.operation(&format!("chunk {name}"));

        group.bench_function(BenchmarkId::from_parameter(&name), |bencher| {
            measured(bencher, &operation, || {
                black_box(compress(format, None, Some(chunk(size)), &input, &memory));
            });
        });
    }

    group.finish();
}

/// Compression levels, so the portable scale's cost across formats is visible rather than assumed.
fn levels(criterion: &mut Criterion, session: &Session) {
    let mut group = criterion.benchmark_group("levels");
    let bytes = payload(64 * 1024);
    group.throughput(Throughput::Bytes(bytes.len() as u64));

    for &format in Format::ALL {
        let memory = GlobalPool::new();
        let input = view(&bytes, &memory);

        for level in [Level::FAST, Level::DEFAULT, Level::BEST] {
            let name = format!("{format:?}/{}", level.get());
            let operation = session.operation(&format!("level {name}"));

            group.bench_function(BenchmarkId::from_parameter(&name), |bencher| {
                measured(bencher, &operation, || {
                    let encoder = format.encoder().level(level);
                    let mut encoder = encoder.build(memory.clone());
                    encoder.push(input.clone()).expect("push succeeds");
                    encoder.finish();

                    let mut total = 0;
                    while let Some(data) = encoder.pull().expect("pull succeeds").into_data() {
                        total += data.len();
                    }

                    black_box(total);
                });
            });
        }
    }

    group.finish();
}

/// Guards the counter-intuitive shape of brotli's window setting.
///
/// Brotli is by far the heaviest allocator here, so shrinking its window looks like an obvious way
/// to trim a service that compresses small messages. Measurement says otherwise: allocation and
/// time both behave as a step function of the window, and *both get worse* below the step, so a
/// small window costs memory and speed at once. The exponents below bracket that step so a change
/// in it is visible rather than silent. The cause lies inside the brotli encoder, so treat these
/// figures as the observed shape rather than as a rule about window sizes in general.
fn brotli_window(criterion: &mut Criterion, session: &Session) {
    let mut group = criterion.benchmark_group("brotli_window");
    let bytes = payload(1024);
    group.throughput(Throughput::Bytes(bytes.len() as u64));

    let memory = GlobalPool::new();
    let input = view(&bytes, &memory);

    for exponent in [10_u8, 16, 18, 22] {
        let window = WindowSize::new(exponent).expect("exponents are in range");
        let name = format!("2^{exponent}");
        let operation = session.operation(&format!("brotli window {name}"));

        group.bench_function(BenchmarkId::from_parameter(&name), |bencher| {
            measured(bencher, &operation, || {
                let mut encoder = brotli::Encoder::builder().window_size(window).build(memory.clone());
                encoder.push(input.clone()).expect("push succeeds");
                encoder.finish();

                let mut total = 0;
                while let Some(data) = encoder.pull().expect("pull succeeds").into_data() {
                    total += data.len();
                }

                black_box(total);
            });
        });
    }

    group.finish();
}

fn benches(criterion: &mut Criterion) {
    // Dropping the session prints the per-iteration allocation table.
    let session = Session::new();

    compression(criterion, &session);
    decompression(criterion, &session);
    pooling(criterion, &session);
    segmentation(criterion, &session);
    chunk_size(criterion, &session);
    levels(criterion, &session);
    brotli_window(criterion, &session);
}

criterion_group!(codec, benches);
criterion_main!(codec);
