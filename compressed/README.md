# compressed ![License: MIT](https://img.shields.io/badge/license-MIT-blue) [![compressed on crates.io](https://img.shields.io/crates/v/compressed)](https://crates.io/crates/compressed) [![compressed on docs.rs](https://docs.rs/compressed/badge.svg)](https://docs.rs/compressed) [![Source Code Repository](https://img.shields.io/badge/Code-On%20GitHub-blue?logo=GitHub)](https://github.com/martintmk/rust-sandbox)

## compressed

Streaming compression and decompression over [`bytesbuf`][__link0] byte sequences. Gzip is the only
format supported today, in the [`gzip`][__link1] module.

Compression engines normally speak `std::io::Read` and `std::io::Write`, which assume a single
contiguous `&[u8]`. A [`BytesView`][__link2] is a chain of segments with no
contiguous representation, so bridging the two through `std::io` would mean copying every byte
into a flat buffer first. This crate drives the engine from the view’s segments directly, and
writes into the uninitialized spare capacity of a [`BytesBuf`][__link3], so no
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

[`gzip::Encoder`][__link4] and [`gzip::Decoder`][__link5] are push/pull state machines rather than one-shot
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

The [`Encoder`][__link6] and [`Decoder`][__link7] traits describe that contract independently of the format, so
callers can be generic over it.

### Security

Gzip can expand its input by orders of magnitude, so a decoder pointed at untrusted data is a
memory-exhaustion vector. [`gzip::Decoder`][__link8] therefore applies
[`DecompressionLimits::DEFAULT`][__link9], which rejects expansion beyond 1000x while placing no cap on
total size, so legitimate large streams still decode. Tighten it with
[`gzip::DecoderBuilder::limits`][__link10], or opt out entirely with [`DecompressionLimits::UNLIMITED`][__link11]
when the source is trusted.

### Features

* `futures-stream` — `CompressStream` and `DecompressStream`, which present compression and
  decompression as a [`futures_core::Stream`][__link12] over any stream of byte sequences.


 [__cargo_doc2readme_dependencies_info]: ggGmYW0CYXZlMC43LjJhdIQb2o_SNWoR6AAb3_T-k0ODPHwbnQW7uS_D2XsbjVFFtK-lC3BhYvVhcoQb5Q6sYArw8TQbycKHXDkj0QIbCW1e_Fqmaa8bHauVTFajHLNhZIOCaGJ5dGVzYnVmZTAuOS4wgmpjb21wcmVzc2VkZTAuMS4wgmxmdXR1cmVzX2NvcmVmMC4zLjM0
 [__link0]: https://crates.io/crates/bytesbuf/0.9.0
 [__link1]: https://docs.rs/compressed/0.1.0/compressed/gzip/index.html
 [__link10]: https://docs.rs/compressed/0.1.0/compressed/?search=gzip::DecoderBuilder::limits
 [__link11]: https://docs.rs/compressed/0.1.0/compressed/?search=DecompressionLimits::UNLIMITED
 [__link12]: https://docs.rs/futures_core/0.3.34/futures_core/?search=Stream
 [__link2]: https://docs.rs/bytesbuf/0.9.0/bytesbuf/?search=BytesView
 [__link3]: https://docs.rs/bytesbuf/0.9.0/bytesbuf/?search=BytesBuf
 [__link4]: https://docs.rs/compressed/0.1.0/compressed/?search=gzip::Encoder
 [__link5]: https://docs.rs/compressed/0.1.0/compressed/?search=gzip::Decoder
 [__link6]: https://docs.rs/compressed/0.1.0/compressed/?search=Encoder
 [__link7]: https://docs.rs/compressed/0.1.0/compressed/?search=Decoder
 [__link8]: https://docs.rs/compressed/0.1.0/compressed/?search=gzip::Decoder
 [__link9]: https://docs.rs/compressed/0.1.0/compressed/?search=DecompressionLimits::DEFAULT
