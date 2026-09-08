# ZIP backend isolation

- `flate2-zlib-ng`: upstream flate2 **1.1.10** from crates.io. Sources are unchanged;
  package/library names are changed to isolate Cargo features. Only source,
  license, README and manifest files are included; example/test targets are removed.
- `zip`: upstream zip **8.6.0** from crates.io. `src/write.rs` adds the
  `NgDeflater` writer variant and `start_file_zlib_ng` method, keeping upstream
  encryption, CRC, ZIP64, metadata and finalization logic. The added dependency
  points to the isolated flate2 package. The normal flate2 dependency uses zlib-rs.

Cargo features are additive. Enabling `zlib-ng` and `zlib-rs` on the same flate2
package selects the C implementation for both callers; dependency aliases do not
isolate features. Distinct package identities let both implementations coexist
without global or thread-local backend selection. Do not replace this with
two aliases of the same crates.io package or enable C backends on normal flate2.

Upstream: https://github.com/rust-lang/flate2-rs and https://github.com/zip-rs/zip2
Licenses are retained in each directory. On upgrades, reapply only the documented
manifest changes and writer extension and run the native ZIP roundtrip tests.
The feature graph can be audited with `cargo tree -e features -i flate2` and
`cargo tree -e features -i flate2-zlib-ng`.

Additional ZIP changes:

- `pooled_deflate.rs`: thread-local allocation caches (one context per backend),
  reset between independent members. Uses flate2's public `Compress` APIs;
  backend choice is explicit for every writer, never a global selector.
- `read.rs`, `read/readers.rs`, `read/zip_archive.rs`, `compression.rs`: per-entry `ZipReadOptions::zlib_ng` selects the
  isolated decoder; `by_index_metadata` avoids payload seeks and decoder creation;
  `has_unicode_name` preserves legacy-codepage decisions.
- `read/config.rs`: optional metadata entry/byte limits bound allocations before
  extraction. Defaults retain upstream behavior.

Run `tests/zip_backends.rs` as well as `tests/sevenzip_roundtrip.rs` after upgrades.

Application ZIP scheduling uses an ordered, bounded compression/merge pipeline.
A slot is refilled only after its spool is merged; at most 32 spools (8 MiB
each before disk spill) exist across running, queued, and merging groups.
Single-file chunk-parallel DEFLATE is not enabled: the existing continuous
stream preserves compression ratio and ZIP64/AES behavior.

## Upgrade review (2026-09-08)

- Updated from crates.io source archives: [zip 8.6.0](https://crates.io/crates/zip/8.6.0)
  and [flate2 1.1.10](https://crates.io/crates/flate2/1.1.10).
  Rebased local changes against upstream 8.2.0; retained the allocation cache,
  explicit backend selection, metadata-only access and metadata limits in the
  new split reader modules. Isolated flate2 source is unchanged from upstream.
- Refreshed Cargo.lock to stable versions compatible with Rust 1.92.
  windows/windows-core use 0.62.2 together for COM macro/type compatibility;
  windows-core 0.100.0 is not compatible with the windows 0.62 series.
- Native runtime releases checked: [libarchive 3.8.9](https://github.com/libarchive/libarchive/releases),
  [7-Zip 26.03](https://www.7-zip.org/), [zlib 1.3.2](https://zlib.net/),
  [XZ 5.8.3](https://tukaani.org/xz/), [LZ4 1.10.0](https://github.com/lz4/lz4/releases),
  and [Zstandard 1.5.7](https://github.com/facebook/zstd/releases) were already current.
  Runtime binaries and their SHA-256 allowlist are unchanged.
- ZIP creation/extraction now default to zlib-rs. Explicit saved preferences,
  including 7z, remain valid. Unknown/missing preferences use the new default.
- Validation: `cargo test --all-targets --locked`; both backend feature trees;
  pipeline ordering, refill while a group is blocked, bounded live results,
  producer/consumer failure, cancellation cleanup, native AES/split roundtrips,
  metadata limits, and multi-window disk-spill roundtrips.

The pipeline removes the batch barrier but preserves source order. A slow next
entry can still stall merging and eventually fill the bounded window. No numeric
speedup is claimed without workload benchmarks. ZIP64 reservation stays conservative.
