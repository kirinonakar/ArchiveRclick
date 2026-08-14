# Rust third-party licenses

This file records the Rust crates used by the locked
`x86_64-pc-windows-msvc` dependency graph for ArchiveRclick. It is an
attribution and license inventory, not a relicensing of these crates. Each
crate remains under the license expression declared by its own package.

ArchiveRclick source code is MIT-licensed. The portable ZIP is a combined
distribution and is not wholly MIT-licensed: bundled Rust crates, native
runtimes,
and their notices retain their own terms.

## Direct dependencies

| Crate | Locked version | Declared license |
| --- | --- | --- |
| `slint` | 1.17.1 | GPL-3.0-only OR Slint Royalty-free 2.0 OR Slint Software 3.0 |
| `slint-build` | 1.17.1 | GPL-3.0-only OR Slint Royalty-free 2.0 OR Slint Software 3.0 |
| `thiserror` | 2.0.20 | MIT OR Apache-2.0 |
| `windows` | 0.61.3 | MIT OR Apache-2.0 |
| `windows-core` | 0.61.2 | MIT OR Apache-2.0 |
| `raw-window-handle` | 0.6.2 | MIT OR Apache-2.0 OR Zlib |
| `sha2` | 0.10.9 | MIT OR Apache-2.0 |
| `embed-resource` | 3.0.9 | MIT |

For Slint 1.17.1, this desktop application uses the Slint Royalty-free
Desktop, Mobile, and Web Applications License and provides the official
attribution badge in the project README. The authoritative license text is
available in the Slint package and at:
<https://github.com/slint-ui/slint/blob/v1.17.1/LICENSE.md>

## Transitive dependency inventory

The Windows x64 dependency graph contains 380 packages including ArchiveRclick;
the following inventory covers its 379 third-party packages. Versions are taken
from `Cargo.lock`; license expressions are the
package-declared values from Cargo metadata. `OR` expressions identify the
alternative terms offered by that package. The package's own license text and
any included NOTICE file remain authoritative.

### (MIT OR Apache-2.0) AND Unicode-3.0
`unicode-ident` 1.0.24

### 0BSD OR MIT OR Apache-2.0
`adler2` 2.0.1

### Apache-2.0
`accesskit_winit` 0.33.2, `clang-sys` 1.9.1, `gl_generator` 0.14.0, `glutin` 0.32.3, `glutin_egl_sys` 0.7.1, `glutin_wgl_sys` 0.6.1, `khronos_api` 3.1.0, `linked_hash_set` 0.1.6, `unicode-linebreak` 0.1.5, `winit` 0.30.13

### Apache-2.0 / MIT
`fnv` 1.0.7

### Apache-2.0 AND MIT
`dpi` 0.1.2

### Apache-2.0 OR MIT
`auto_enums` 0.8.10, `autocfg` 1.5.1, `derive_utils` 0.16.0, `equivalent` 1.0.2, `fontique` 0.10.0, `idna_adapter` 1.2.2, `indexmap` 2.14.0, `kurbo` 0.13.1, `linebender_resource_handle` 0.1.1, `muda` 0.19.3, `no_std_io2` 0.9.4, `parlance` 0.1.0, `parley` 0.10.0, `parley_data` 0.10.0, `pin-project` 1.1.13, `pin-project-internal` 1.1.13, `pin-project-lite` 0.2.17, `portable-atomic` 1.15.0, `resvg` 0.47.0, `rustc-hash` 2.1.3, `simplecss` 0.2.2, `spin_on` 0.1.1, `svgtypes` 0.16.1, `swash` 0.2.10, `usvg` 0.47.0, `utf8_iter` 1.0.4, `uuid` 1.24.0, `yazi` 0.2.1, `zeno` 0.3.3

### Apache-2.0/MIT
`bit_field` 0.10.3, `cexpr` 0.6.0, `integer-sqrt` 0.1.5, `rustc-hash` 1.1.0

### BSD-2-Clause
`arrayref` 0.3.9, `av1-grain` 0.2.5, `rav1e` 0.8.1, `v_frame` 0.3.9

### BSD-2-Clause OR Apache-2.0 OR MIT
`zerocopy` 0.8.56, `zerocopy-derive` 0.8.56

### BSD-3-Clause
`avif-serialize` 0.8.9, `bindgen` 0.72.1, `exr` 1.74.2, `lebe` 0.5.3, `ravif` 0.13.0, `tiny-skia` 0.12.0, `tiny-skia-path` 0.12.0

### BSD-3-Clause OR Apache-2.0
`moxcms` 0.8.1, `pxfm` 0.1.30

### BSD-3-Clause OR MIT OR Apache-2.0
`num_enum` 0.7.6, `num_enum_derive` 0.7.6

### BSL-1.0
`clipboard-win` 5.4.1, `error-code` 3.3.2

### CC0-1.0 OR Apache-2.0
`imgref` 1.12.2

### GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0
`i-slint-backend-selector` 1.17.1, `i-slint-backend-winit` 1.17.1, `i-slint-common` 1.17.1, `i-slint-compiler` 1.17.1, `i-slint-core` 1.17.1, `i-slint-core-macros` 1.17.1, `i-slint-renderer-femtovg` 1.17.1, `i-slint-renderer-skia` 1.17.1, `i-slint-renderer-software` 1.17.1, `slint` 1.17.1, `slint-build` 1.17.1, `slint-macros` 1.17.1

### ISC
`libloading` 0.8.9

### MIT
`aligned-vec` 0.6.4, `arg_enum_proc_macro` 0.3.4, `av-scenechange` 0.14.1, `bincode` 2.0.1, `built` 0.8.1, `bytes` 1.12.1, `cfg_aliases` 0.2.2, `clru` 0.6.3, `color_quant` 1.1.0, `convert_case` 0.10.0, `core_maths` 0.1.1, `derive_more` 2.1.1, `derive_more-impl` 2.1.1, `embed-resource` 3.0.9, `equator` 0.4.2, `equator-macro` 0.4.2, `fax` 0.2.7, `float-cmp` 0.9.0, `fontdb` 0.23.0, `generic-array` 0.14.7, `glutin-winit` 0.5.0, `grid` 1.0.1, `harfrust` 0.8.4, `imagesize` 0.14.0, `libm` 0.2.16, `loop9` 0.1.5, `maybe-rayon` 0.1.1, `memoffset` 0.9.1, `natord` 1.0.9, `new_debug_unreachable` 1.0.6, `nom` 7.1.3, `nom` 8.0.0, `noop_proc_macro` 0.3.0, `pico-args` 0.5.0, `pin-weak` 1.1.0, `pulldown-cmark` 0.13.4, `pulldown-cmark-escape` 0.11.0, `pulp` 0.22.3, `pulp-wasm-simd-flag` 0.1.1, `raw-cpuid` 11.6.0, `reborrow` 0.5.5, `rgb` 0.8.53, `rspolib` 0.1.2, `rustybuzz` 0.20.1, `simd_helpers` 0.1.0, `simd-adler32` 0.3.10, `skia-bindings` 0.99.0, `skia-safe` 0.99.0, `slab` 0.4.12, `strict-num` 0.1.1, `strum` 0.28.0, `strum_macros` 0.28.0, `synstructure` 0.13.2, `taffy` 0.10.1, `tiff` 0.11.3, `tracing` 0.1.44, `tracing-attributes` 0.1.31, `tracing-core` 0.1.36, `vswhom` 0.1.0, `vswhom-sys` 0.1.3, `winnow` 1.0.4, `winreg` 0.55.0, `xml-rs` 0.8.29, `xmlwriter` 0.1.0, `y4m` 0.8.0, `zmij` 1.0.23

### MIT / Apache-2.0
`copypasta` 0.10.2

### MIT OR Apache-2.0
`accesskit` 0.24.1, `accesskit_consumer` 0.38.0, `accesskit_windows` 0.34.0, `aligned` 0.4.3, `allocator-api2` 0.2.21, `annotate-snippets` 0.12.16, `anstyle` 1.0.14, `anyhow` 1.0.104, `arrayvec` 0.7.8, `as-slice` 0.2.1, `base64` 0.22.1, `bitflags` 2.13.1, `block-buffer` 0.10.4, `borsh` 1.8.0, `bumpalo` 3.20.3, `by_address` 1.2.1, `cc` 1.4.2, `cfg-if` 1.0.4, `chrono` 0.4.45, `const-field-offset` 0.2.0, `const-field-offset-macro` 0.2.0, `countme` 3.0.1, `cpufeatures` 0.2.17, `crc32fast` 1.5.0, `critical-section` 1.2.0, `crossbeam-channel` 0.5.16, `crossbeam-deque` 0.8.7, `crossbeam-epoch` 0.9.20, `crossbeam-utils` 0.8.22, `crypto-common` 0.1.7, `data-url` 0.3.2, `digest` 0.10.7, `displaydoc` 0.2.7, `either` 1.17.0, `euclid` 0.22.14, `fdeflate` 0.3.7, `femtovg` 0.25.1, `field-offset` 0.3.6, `find-msvc-tools` 0.1.10, `flate2` 1.1.9, `font-types` 0.11.3, `font-types` 0.12.1, `form_urlencoded` 1.2.2, `getopts` 0.2.24, `getrandom` 0.4.3, `gif` 0.14.2, `glob` 0.3.4, `half` 2.7.1, `hashbrown` 0.14.5, `hashbrown` 0.16.1, `hashbrown` 0.17.1, `heck` 0.5.0, `htmlparser` 0.2.1, `idna` 1.1.0, `image` 0.25.10, `image-webp` 0.2.4, `itertools` 0.13.0, `itertools` 0.14.0, `itoa` 1.0.18, `jobserver` 0.1.35, `keyboard-types` 0.7.0, `lazy_static` 1.5.0, `libc` 0.2.189, `log` 0.4.33, `lyon_algorithms` 1.0.20, `lyon_extra` 1.1.0, `lyon_geom` 1.0.19, `lyon_path` 1.0.19, `memmap2` 0.9.11, `num-bigint` 0.4.8, `num-complex` 0.4.6, `num-derive` 0.4.2, `num-integer` 0.1.47, `num-rational` 0.4.2, `num-traits` 0.2.19, `once_cell` 1.21.4, `paste` 1.0.15, `pastey` 0.1.1, `percent-encoding` 2.3.2, `pin-utils` 0.1.0, `pkg-config` 0.3.33, `png` 0.18.1, `polycool` 0.4.0, `prettyplease` 0.2.37, `proc-macro-crate` 3.5.0, `proc-macro2` 1.0.107, `profiling` 1.0.18, `profiling-procmacros` 1.0.18, `quote` 1.0.47, `rayon` 1.12.0, `rayon-core` 1.13.0, `read-fonts` 0.39.2, `read-fonts` 0.41.0, `regex` 1.13.1, `regex-automata` 0.4.18, `regex-syntax` 0.8.11, `rowan` 0.16.1, `roxmltree` 0.21.1, `rustc_version` 0.4.1, `rustversion` 1.0.23, `scopeguard` 1.2.0, `semver` 1.0.28, `serde` 1.0.229, `serde_core` 1.0.229, `serde_derive` 1.0.229, `serde_json` 1.0.151, `serde_spanned` 1.1.1, `sha2` 0.10.9, `shlex` 1.3.0, `shlex` 2.0.1, `skrifa` 0.42.1, `skrifa` 0.44.0, `smallvec` 1.15.2, `smol_str` 0.2.2, `smol_str` 0.3.2, `snafu` 0.8.9, `snafu-derive` 0.8.9, `softbuffer` 0.4.8, `stable_deref_trait` 1.2.1, `static_assertions` 1.1.0, `syn` 2.0.119, `syn` 3.0.3, `sys-locale` 0.3.2, `tar` 0.4.46, `text-size` 1.1.1, `thiserror` 2.0.20, `thiserror-impl` 2.0.20, `toml` 1.1.4+spec-1.1.0, `toml_datetime` 1.1.1+spec-1.1.0, `toml_edit` 0.25.13+spec-1.1.0, `toml_parser` 1.1.3+spec-1.1.0, `toml_writer` 1.1.2+spec-1.1.0, `ttf-parser` 0.25.1, `typed-index-collections` 3.3.0, `typenum` 1.20.1, `unicase` 2.9.0, `unicode-bidi` 0.3.18, `unicode-script` 0.5.8, `unicode-segmentation` 1.13.3, `unicode-width` 0.2.2, `unicode-xid` 0.2.6, `unty` 0.0.4, `url` 2.5.8, `vtable` 0.4.0, `vtable-macro` 0.4.0, `wasm-bindgen` 0.2.127, `wasm-bindgen-macro` 0.2.127, `wasm-bindgen-macro-support` 0.2.127, `wasm-bindgen-shared` 0.2.127, `webbrowser` 1.2.4, `weezl` 0.1.12, `windows` 0.61.3, `windows` 0.62.2, `windows_x86_64_msvc` 0.52.6, `windows-collections` 0.2.0, `windows-collections` 0.3.2, `windows-core` 0.61.2, `windows-core` 0.62.2, `windows-future` 0.2.1, `windows-future` 0.3.2, `windows-implement` 0.60.2, `windows-interface` 0.59.3, `windows-link` 0.1.3, `windows-link` 0.2.1, `windows-numerics` 0.2.0, `windows-numerics` 0.3.1, `windows-result` 0.3.4, `windows-result` 0.4.1, `windows-strings` 0.4.2, `windows-strings` 0.5.1, `windows-sys` 0.52.0, `windows-sys` 0.59.0, `windows-sys` 0.61.2, `windows-targets` 0.52.6, `windows-threading` 0.1.0, `windows-threading` 0.2.1

### MIT OR Apache-2.0 OR Zlib
`cursor-icon` 1.2.0, `glow` 0.17.0, `raw-window-handle` 0.6.2, `tinyvec_macros` 0.1.1, `zune-core` 0.5.3, `zune-inflate` 0.2.54, `zune-jpeg` 0.5.15

### MIT OR Zlib OR Apache-2.0
`miniz_oxide` 0.8.9

### MIT/Apache-2.0
`bitstream-io` 4.10.0, `filetime` 0.2.29, `linked-hash-map` 0.5.6, `minimal-lexical` 0.2.1, `qoi` 0.4.1, `quick-error` 2.0.1, `scoped-tls-hkt` 0.1.5, `siphasher` 1.0.3, `unicode-bidi-mirroring` 0.4.0, `unicode-ccc` 0.4.0, `unicode-properties` 0.1.4, `unicode-vo` 0.1.0, `version_check` 0.9.5

### Unicode-3.0
`fixed_decimal` 0.7.2, `icu_collections` 2.2.0, `icu_decimal` 2.2.0, `icu_decimal_data` 2.2.0, `icu_locale` 2.2.0, `icu_locale_core` 2.2.0, `icu_locale_data` 2.2.0, `icu_normalizer` 2.2.0, `icu_normalizer_data` 2.2.0, `icu_plurals` 2.2.0, `icu_plurals_data` 2.2.0, `icu_properties` 2.2.0, `icu_properties_data` 2.2.0, `icu_provider` 2.2.0, `icu_segmenter` 2.2.0, `icu_segmenter_data` 2.2.0, `litemap` 0.8.2, `potential_utf` 0.1.5, `tinystr` 0.8.3, `writeable` 0.6.3, `yoke` 0.8.3, `yoke-derive` 0.8.2, `zerofrom` 0.1.8, `zerofrom-derive` 0.1.7, `zerotrie` 0.2.4, `zerovec` 0.11.6, `zerovec-derive` 0.11.3

### Unlicense OR MIT
`aho-corasick` 1.1.5, `byteorder-lite` 0.1.0, `memchr` 2.8.3

### Zlib
`foldhash` 0.2.0, `slotmap` 1.1.1

### Zlib OR Apache-2.0 OR MIT
`bytemuck` 1.25.2, `bytemuck_derive` 1.12.0, `tinyvec` 1.12.0

## Native runtime notices

The separately bundled libarchive, codec, 7-Zip, and Microsoft runtime files
are documented in
[`runtime/THIRD-PARTY-NOTICES.md`](runtime/THIRD-PARTY-NOTICES.md), with full
native license texts in [`runtime/licenses/`](runtime/licenses/).
