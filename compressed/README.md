# compressed ![License: MIT](https://img.shields.io/badge/license-MIT-blue) [![compressed on crates.io](https://img.shields.io/crates/v/compressed)](https://crates.io/crates/compressed) [![compressed on docs.rs](https://docs.rs/compressed/badge.svg)](https://docs.rs/compressed) [![Source Code Repository](https://img.shields.io/badge/Code-On%20GitHub-blue?logo=GitHub)](https://github.com/martintmk/rust-sandbox)

Streaming compression and decompression over [`bytesbuf`][__link0] byte sequences.

Five formats are available, each behind a cargo feature of its own: `deflate`, `zlib`,
`gzip`, `brotli` and `zstd`. Each lives in its own module and exposes the same six items,
so moving between them is a change of import rather than a change of code.

Compression engines normally speak `std::io::Read` and `std::io::Write`, which assume a single
contiguous `&[u8]`. A [`BytesView`][__link1] is a chain of segments with no
contiguous representation, so bridging the two through `std::io` would mean copying every byte
into a flat buffer first. This crate drives the engine from the view’s segments directly, and
writes into the uninitialized spare capacity of a [`BytesBuf`][__link2], so no
intermediate copy is needed.

## Whole buffers

```rust
use bytesbuf::BytesView;
use bytesbuf::mem::GlobalPool;
use compressed::gzip;

let memory = GlobalPool::new();
let encoded = gzip::compress(
    BytesView::copied_from_slice(b"hello", &memory),
    memory.clone(),
)?;

assert_eq!(
    gzip::decompress(encoded, memory)?.to_vec(),
    b"hello".to_vec()
);
```

## Streaming

[`gzip::Encoder`][__link3] and [`gzip::Decoder`][__link4] are push/pull state machines rather than one-shot
transforms. Each `pull` returns at most one chunk, so processing a multi-gigabyte stream never
holds more than one pending input view plus one output chunk:

```rust
use bytesbuf::mem::GlobalPool;
use bytesbuf::{BytesBuf, BytesView};
use compressed::{Output, gzip};

let mut decoder = gzip::Decoder::new(memory);
let mut chunks = source.into_iter();
let mut plain = BytesBuf::new();

loop {
    match decoder.pull()? {
        Output::Data(data) => plain.put_bytes(data),
        Output::NeedInput => match chunks.next() {
            Some(chunk) => decoder.push(chunk)?,
            None => decoder.finish(),
        },
        Output::Done => break,
    }
}

assert_eq!(plain.consume_all().to_vec(), b"streamed".to_vec());
```

## Choosing a format

The [`Encoder`][__link5] and [`Decoder`][__link6] traits describe the contract independently of the format, so
code can be written once and used with any of them. When the format is only known at runtime —
from a `Content-Encoding` header, say — [`format::Format`][__link7] resolves it and its builders produce a boxed
codec, which is itself an `Encoder` or `Decoder` and so fits anywhere a concrete one does:

```rust
use bytesbuf::BytesView;
use bytesbuf::mem::GlobalPool;
use compressed::Level;
use compressed::format::Format;

// Pick the encoding the client ranked highest among those this build supports.
let format = Format::from_accept_encoding("br;q=1.0, gzip;q=0.8, deflate;q=0.5")
    .next()
    .expect("no mutually supported encoding");

let memory = GlobalPool::new();
let encoded = format.compress(
    BytesView::copied_from_slice(b"negotiated", &memory),
    memory.clone(),
)?;

assert_eq!(
    format.decompress(encoded, memory)?.to_vec(),
    b"negotiated".to_vec()
);
```

## Reusing engine state

Building a compressor allocates and initialises a substantial amount of state — on a small
message, as much work as the compression itself. A service that encodes many messages should
hold one [`Pool`][__link8], clone it into each encoder, and let the engine return to the pool when the
encoder drops. The saving is roughly fixed per message, so it matters most for small bodies.

```rust
use bytesbuf::mem::GlobalPool;
use compressed::{Pool, gzip};

let codecs = Pool::new();
let memory = GlobalPool::new();

// Per request: cheap to build, recycles the engine on drop.
let encoder = gzip::Encoder::builder().pool(codecs.clone()).build(memory);
```

The pool is transparent — it recycles what is worth recycling and builds the rest — so calling
code never has to know which engines benefit. See [`Pool`][__link9] for what is pooled today.

## Security

Every one of these formats can expand its input by orders of magnitude, so a decoder pointed at
untrusted data is a memory-exhaustion vector.

The codecs themselves never accumulate: each `pull` hands back one bounded chunk, so nothing in
this crate grows with the length of the stream. The exposure belongs to whatever the caller does
with those chunks, which is why the limits matter most for the accumulating conveniences —
`compress`, `decompress`, and [`format::Format::compress`][__link10] / [`format::Format::decompress`][__link11].

Each format declares its own default bounds, because a single portable ratio cannot serve both
families. Deflate cannot expand by more than about 1032x — a structural property of the format —
so the deflate family defaults to 1100x and never rejects data it could legitimately have
produced. Brotli has no such ceiling: measured on ordinary repetitive input it reaches 9 000x
for a repeated short string, 21 000x for a repeated sentence and 80 660x for a megabyte of
zeros, so it defaults to 250 000x.

[`DecompressionLimits`][__link12] carries *overrides*, not values: bounds you leave unset keep the
format’s default, so [`DecompressionLimits::default()`][__link13] never silently imposes one format’s
calibration on another.

**A ratio limit is therefore a coarse backstop, not real protection.** For untrusted input, set
[`DecompressionLimits::with_max_output_len`][__link14] to whatever the caller can actually afford to
buffer. Use [`DecompressionLimits::UNLIMITED`][__link15] only for sources you trust as much as your own
process.

## Features

Every format is a separate feature, so a build compiles only the engines it names:

* `gzip` — the `gzip` module and `Format::Gzip`, via `flate2`. The only feature on by
  default, being the encoding most often seen on the wire.
* `deflate` — the `deflate` module and `Format::Deflate`, via `flate2`.
* `zlib` — the `zlib` module and `Format::Zlib`, via `flate2`.
* `brotli` — the `brotli` module and `Format::Brotli`, via the pure-Rust `brotli` crate.
* `zstd` — the `zstd` module and `Format::Zstd`, via `zstd-safe`.
* `futures-stream` — the `stream` module, presenting compression and decompression as a
  `futures_core::Stream` over any stream of byte sequences.

The deflate-family features share one dependency, so enabling all three costs no more than one.
A build that needs only `brotli` or only `zstd` never compiles `flate2` at all.


 [__cargo_doc2readme_dependencies_info]: ggGmYW0CYXZlMC43LjJhdIQb2o_SNWoR6AAb3_T-k0ODPHwbnQW7uS_D2XsbjVFFtK-lC3BhYvVhcoQbj3Hl1rrs_sobZ1IUJJYazA8bnP7KYcDLTC0bZWjznYcnHYJhZIKCaGJ5dGVzYnVmZTAuOS4wgmpjb21wcmVzc2VkZTAuMS4w
 [__link0]: https://crates.io/crates/bytesbuf/0.9.0
 [__link1]: https://docs.rs/bytesbuf/0.9.0/bytesbuf/?search=BytesView
 [__link10]: https://docs.rs/compressed/0.1.0/compressed/?search=format::Format::compress
 [__link11]: https://docs.rs/compressed/0.1.0/compressed/?search=format::Format::decompress
 [__link12]: https://docs.rs/compressed/0.1.0/compressed/?search=DecompressionLimits
 [__link13]: https://docs.rs/compressed/0.1.0/compressed/?search=DecompressionLimits::default
 [__link14]: https://docs.rs/compressed/0.1.0/compressed/?search=DecompressionLimits::with_max_output_len
 [__link15]: https://docs.rs/compressed/0.1.0/compressed/?search=DecompressionLimits::UNLIMITED
 [__link2]: https://docs.rs/bytesbuf/0.9.0/bytesbuf/?search=BytesBuf
 [__link3]: https://docs.rs/compressed/0.1.0/compressed/?search=gzip::Encoder
 [__link4]: https://docs.rs/compressed/0.1.0/compressed/?search=gzip::Decoder
 [__link5]: https://docs.rs/compressed/0.1.0/compressed/?search=Encoder
 [__link6]: https://docs.rs/compressed/0.1.0/compressed/?search=Decoder
 [__link7]: https://docs.rs/compressed/0.1.0/compressed/?search=format::Format
 [__link8]: https://docs.rs/compressed/0.1.0/compressed/?search=Pool
 [__link9]: https://docs.rs/compressed/0.1.0/compressed/?search=Pool
