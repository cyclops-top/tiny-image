# Validation record (2026-10-23, tinyimg 0.2.0)

Verified on x86_64 Linux (Debian 13), Rust 1.99.1, default locked dependency features:

- `make check`: rustfmt and workspace/all-target Clippy with warnings denied passed.
- `make test`: 104 core unit tests, 10 CLI progress unit tests, and 35 CLI integration tests passed (149 total).
- `cargo deny check licenses sources`: passed with cargo-deny 0.20.2.
- `cargo about generate --locked --workspace --fail --format json -o THIRD-PARTY-LICENSES.json`: passed with cargo-about 0.9.2 (`--features cli` required when installing it). Machine-local path metadata was normalized with `scripts/sanitize_license_inventory.py`; full license texts remain unchanged.
- `make release`: Linux release executable built successfully.
- Release CLI `--help`, `--version`, and actual AVIF output writes completed successfully. Valid, synthetic 64x48 PNGs produced a 588-byte transparent AVIF with measured SSIMULACRA2 80.6732 and a 343-byte opaque AVIF with measured score 80.8810, both meeting target 80. These are roundtrip smoke tests, not quality calibration or representative performance benchmarks.

The included Linux executable dynamically links glibc, libm and libgcc_s; this build references GLIBC_2.39. It is not a universal Linux/static binary. Build from source on older distributions.

The suite exercises real PNG/WebP/JPEG/AVIF roundtrips, final-byte measured SSIMULACRA2 and exact lossless identity, transparency, metadata restrictions, quality search, conversion savings, Android manifests/density rules, no-write modes, output preservation, rollback/no-clobber, bounded source reads, Unicode/spaces, Unix non-UTF-8 JSON paths, and root aliases versus descendant symlinks.

Human-readable formatting tests cover decimal unit boundaries and rounding, zero savings, exact byte-derived savings percentages, maximum u64 sizes, per-file/candidate/summary output, and unchanged numeric JSON fields. Spinner selection tests cover UTF-8 braille dots, legacy/non-UTF-8 ASCII fallback, single-column animation frames, CJK display-width column alignment, narrow multiline layouts, status colors, NO_COLOR/dumb-terminal gating, and escaping filenames before styling.

Progress tests cover real candidate phase/attempt order and output equivalence, bounded active rows, hidden-row promotion, out-of-order completion counts, one completion per skip/error, terminal-control escaping, and TTY/JSON/quiet gating. Actual Linux pseudo-terminal smoke checks additionally exercise four concurrent images, a single image, a narrow 30x7 terminal, cancellation before any animation, JSON/quiet output, and independent stdout/stderr redirection. Completed result records are emitted once; active rows are cleared before the final summary.

The directory-confirmation tests cover y/yes, n, empty input, EOF, mixed directory/file inputs, zero eligible images, size/filter protections, quiet mode, clean JSON stdout, output and read-only bypasses, and adding a new image while waiting for confirmation (the new image is not processed). The target/release regression invokes relative `编组 (2).png` and `test.png` from inside that directory; both are processed, while nested default-excluded trees remain pruned.

Processed-output markers have 17 module tests plus end-to-end tests for no repeated same-settings work, settings invalidation, output-only cache placement, corruption/symlink/size safeguards, and strict no-write modes. Markers store neither image bytes nor historical scores. External AVIF inputs are rejected before AV1 decoding; generated AVIF is output-only, with a 4-million-pixel cap and globally serialized verification.

Windows/macOS native code paths and Make targets exist, including Windows-only path tests, but the entire application was not built or run on those systems here. Isolated pure-Rust AVIF dependency proofs passed `cargo check` for x86_64-pc-windows-msvc, aarch64-apple-darwin and x86_64-apple-darwin; this does not establish whole-application linking or runtime behavior. The optional manual CI jobs let maintainers validate native platforms later.

No claims are made about a 100–200-image visual calibration, thousand-image performance, a global mathematical optimum, full ICC/EXIF transformation, arbitrary AVIF import, or a persistent cache of reusable encoded candidates. Presets remain explicitly experimental.
