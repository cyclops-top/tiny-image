# Validation record (2026-10-23)

Verified on x86_64 Linux (Debian 13), Rust 1.99.1, default locked dependency features:

- `make check`: rustfmt and workspace/all-target clippy with warnings denied passed.
- `make test`: 71 core unit tests and 16 CLI integration tests passed (87 total).
- `cargo deny check licenses sources`: passed with cargo-deny 0.20.2.
- `cargo about generate --locked --workspace --fail --format json -o THIRD-PARTY-LICENSES.json`: passed with cargo-about 0.9.2 (`--features cli` required when installing it). Machine-local path metadata was then normalized with `scripts/sanitize_license_inventory.py`; license texts remain unchanged.
- `make release`: Linux release executable built successfully.
- Release CLI `--help`, `--version`, and a two-image PNG/WebP/JPEG dry-run completed successfully. The synthetic 128x128 fixtures are smoke tests, not quality-calibration data or representative performance benchmarks.

The included Linux executable dynamically links glibc, libm and libgcc_s; this build references GLIBC_2.39. It is not a universal Linux/static binary. Build from source on older distributions.

The test suite exercises real codec roundtrips, measured SSIMULACRA2 and exact lossless identity, transparency, metadata restrictions, quality search, conversion savings, Android manifests/density rules, no-write modes, output preservation, rollback/no-clobber, Unicode/spaces, Unix non-UTF-8 JSON paths, and root aliases versus descendant symlinks.

Windows/macOS code paths and Make targets exist, including Windows-only path tests. They were not executed here. The optional manual CI jobs let maintainers validate those platforms later. No claims are made about a 100–200-image visual calibration, thousand-image performance, a global mathematical optimum, or AVIF/caching support in this baseline.
