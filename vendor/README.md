# ZIP backend isolation

- `flate2-zlib-ng`: upstream flate2 **1.1.9** from crates.io. Sources are unchanged;
  package/library names are changed to isolate Cargo features. Only source,
  license, README and manifest files are included; example/test targets are removed.
- `zip`: upstream zip **8.2.0** from crates.io. `src/write.rs` adds the
  `NgDeflater` writer variant and `start_file_zlib_ng` method, keeping upstream
  encryption, CRC, ZIP64, metadata and finalization logic. The added dependency
  points to the isolated flate2 package. The normal flate2 dependency uses zlib-rs.

Cargo features are additive. Enabling `zlib-ng` and `zlib-rs` on the same flate2
package selects the C implementation for both callers; dependency aliases do not
isolate features. Distinct package identities let both implementations coexist
without global or thread-local mutable backend state. Do not replace this with
two aliases of the same crates.io package or enable C backends on normal flate2.

Upstream: https://github.com/rust-lang/flate2-rs and https://github.com/zip-rs/zip2
Licenses are retained in each directory. On upgrades, reapply only the documented
manifest changes and writer extension and run the native ZIP roundtrip tests.
The feature graph can be audited with `cargo tree -e features -i flate2` and
`cargo tree -e features -i flate2-zlib-ng`.
