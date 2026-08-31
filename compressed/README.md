# compressed ![License: MIT](https://img.shields.io/badge/license-MIT-blue) [![compressed on crates.io](https://img.shields.io/crates/v/compressed)](https://crates.io/crates/compressed) [![compressed on docs.rs](https://docs.rs/compressed/badge.svg)](https://docs.rs/compressed) [![Source Code Repository](https://img.shields.io/badge/Code-On%20GitHub-blue?logo=GitHub)](https://github.com/martintmk/rust-sandbox)

## compressed

Streaming compression and decompression over [`bytesbuf`][__link0] byte sequences.

Four formats: [`deflate`][__link1], [`zlib`][__link2], [`gzip`][__link3], and [`brotli`][__link4] behind the `brotli` feature.
Each lives in its own module and exposes the same six items, so moving between them is a change
of import rather than a change of code.

Compression engines normally speak `std::io::Read` and `std::io::Write`, which assume a single
contiguous `&[u8]`. A [`BytesView`][__link5] is a chain of segments with no
contiguous representation, so bridging the two through `std::io` would mean copying every byte
into a flat buffer first. This crate drives the engine from the view’s segments directly, and
writes into the uninitialized spare capacity of a [`BytesBuf`][__link6], so no
intermediate copy is needed.

### Whole buffers

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

### Streaming

[`gzip::Encoder`][__link7] and [`gzip::Decoder`][__link8] are push/pull state machines rather than one-shot
transforms. Each `pull` returns at most one chunk, so processing a multi-gigabyte stream never
holds more than one pending input view plus one output chunk:

```rust
use bytesbuf::BytesView;
use bytesbuf::mem::GlobalPool;
use compressed::{Output, gzip};

let mut decoder = gzip::Decoder::new(memory);
let mut chunks = source.into_iter();
let mut plain = Vec::new();

loop {
    match decoder.pull()? {
        Output::Data(data) => plain.push(data),
        Output::NeedInput => match chunks.next() {
            Some(chunk) => decoder.push(chunk)?,
            None => decoder.finish(),
        },
        Output::Done => break,
    }
}

assert_eq!(BytesView::from_views(plain).to_vec(), b"streamed".to_vec());
```

### Choosing a format

The [`Encoder`][__link9] and [`Decoder`][__link10] traits describe the contract independently of the format, so
code can be written once and used with any of them. When the format is only known at runtime —
from a `Content-Encoding` header, say — [`Format`][__link11] resolves it and its builders produce a boxed
codec, which is itself an `Encoder` or `Decoder` and so fits anywhere a concrete one does:

```rust
use bytesbuf::BytesView;
use bytesbuf::mem::GlobalPool;
use compressed::{Format, Level};

// Pick the first encoding the client offered that this build supports.
let format = "br, gzip, deflate"
    .split(',')
    .find_map(Format::from_content_encoding)
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

### Reusing engine state

Building a compressor allocates and initialises a substantial amount of state — on a small
message, as much work as the compression itself. A service that encodes many messages should
hold one [`Pool`][__link12], clone it into each encoder, and let the engine return to the pool when the
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
code never has to know which engines benefit. See [`Pool`][__link13] for what is pooled today.

### Security

Every one of these formats can expand its input by orders of magnitude, so a decoder pointed at
untrusted data is a memory-exhaustion vector.

The codecs themselves never accumulate: each `pull` hands back one bounded chunk, so nothing in
this crate grows with the length of the stream. The exposure belongs to whatever the caller does
with those chunks, which is why the limits matter most for the accumulating conveniences —
`compress`, `decompress`, and [`Format::compress`][__link14] / [`Format::decompress`][__link15].

Each format declares its own default bounds, because a single portable ratio cannot serve both
families. Deflate cannot expand by more than about 1032x — a structural property of the format —
so the deflate family defaults to 1100x and never rejects data it could legitimately have
produced. Brotli has no such ceiling: measured on ordinary repetitive input it reaches 9 000x
for a repeated short string, 21 000x for a repeated sentence and 80 660x for a megabyte of
zeros, so it defaults to 250 000x.

[`DecompressionLimits`][__link16] carries *overrides*, not values: bounds you leave unset keep the
format’s default, so [`DecompressionLimits::default()`][__link17] never silently imposes one format’s
calibration on another.

**A ratio limit is therefore a coarse backstop, not real protection.** For untrusted input, set
[`DecompressionLimits::with_max_output_len`][__link18] to whatever the caller can actually afford to
buffer. Use [`DecompressionLimits::UNLIMITED`][__link19] only for sources you trust as much as your own
process.

### Features

* `brotli` — the [`brotli`][__link20] module and [`Format::Brotli`][__link21], via the pure-Rust `brotli` crate.
* `futures-stream` — `CompressStream` and `DecompressStream`, which present compression and
  decompression as a [`futures_core::Stream`][__link22] over any stream of byte sequences.

Both are off by default, so the base build pulls in nothing beyond `bytesbuf` and `flate2`.


 [__cargo_doc2readme_dependencies_info]: ggGmYW0CYXZlMC43LjJhdIQb2o_SNWoR6AAb3_T-k0ODPHwbnQW7uS_D2XsbjVFFtK-lC3BhYvVhcoQbvIzNqjnIlvIbrsJDhS0yybEbIrrFWZM_QS0bXl7uUU3HKWRhZIOCaGJ5dGVzYnVmZTAuOS4wgmpjb21wcmVzc2VkZTAuMS4wgmxmdXR1cmVzX2NvcmVmMC4zLjM0
 [__link0]: https://crates.io/crates/bytesbuf/0.9.0
 [__link1]: https://docs.rs/compressed/0.1.0/compressed/deflate/index.html
 [__link10]: https://docs.rs/compressed/0.1.0/compressed/?search=Decoder
 [__link11]: https://docs.rs/compressed/0.1.0/compressed/?search=Format
 [__link12]: https://docs.rs/compressed/0.1.0/compressed/?search=Pool
 [__link13]: https://docs.rs/compressed/0.1.0/compressed/?search=Pool
 [__link14]: https://docs.rs/compressed/0.1.0/compressed/?search=Format::compress
 [__link15]: https://docs.rs/compressed/0.1.0/compressed/?search=Format::decompress
 [__link16]: https://docs.rs/compressed/0.1.0/compressed/?search=DecompressionLimits
 [__link17]: https://docs.rs/compressed/0.1.0/compressed/?search=DecompressionLimits::default
 [__link18]: https://docs.rs/compressed/0.1.0/compressed/?search=DecompressionLimits::with_max_output_len
 [__link19]: https://docs.rs/compressed/0.1.0/compressed/?search=DecompressionLimits::UNLIMITED
 [__link2]: https://docs.rs/compressed/0.1.0/compressed/zlib/index.html
 [__link20]: https://docs.rs/compressed/0.1.0/compressed/brotli/index.html
 [__link21]: https://docs.rs/compressed/0.1.0/compressed/?search=Format::Brotli
 [__link22]: https://docs.rs/futures_core/0.3.34/futures_core/?search=Stream
 [__link3]: https://docs.rs/compressed/0.1.0/compressed/gzip/index.html
 [__link4]: https://docs.rs/compressed/0.1.0/compressed/brotli/index.html
 [__link5]: https://docs.rs/bytesbuf/0.9.0/bytesbuf/?search=BytesView
 [__link6]: https://docs.rs/bytesbuf/0.9.0/bytesbuf/?search=BytesBuf
 [__link7]: https://docs.rs/compressed/0.1.0/compressed/?search=gzip::Encoder
 [__link8]: https://docs.rs/compressed/0.1.0/compressed/?search=gzip::Decoder
 [__link9]: https://docs.rs/compressed/0.1.0/compressed/?search=Encoder
