# avif-decode 3.0.0 vendoring record

Source: the official crates.io archive https://crates.io/api/v1/crates/avif-decode/3.0.0/download

Archive SHA-256: `1eee11893966d77d3e18c66fdcfb1c30e2d8df050346609c775ce8eec3390344`. Upstream repository: https://github.com/kornelski/avif-decode

Retained upstream library/CLI sources, regression tests and fixture, original manifest, normalized manifest, README, and the unmodified BSD-3-Clause LICENSE. The upstream lockfile and Cargo cache metadata are not needed by this workspace and are omitted.

Local changes:

1. Cargo.toml: disable rav1d default features on x86/x86_64 and enable only bitdepth_8 and bitdepth_16, matching upstream non-x86 configuration. This removes the NASM build requirement; rav1d's Rust decoder remains real AV1 decoding. Cargo.toml.orig records the untouched upstream manifest.
2. src/lib.rs: cap decoder threads at one instead of available_parallelism capped at 32. tinyimg also serializes AVIF encoding/verification with a global mutex.

No codec algorithm changes were made. ravif and rav1d assembly features are disabled. This is portable Rust codec code, not a claim that the entire application lacks other native C dependencies.

## Scope and upstream limitation

The decoder is used only for freshly generated in-process candidates. External AVIF inputs are explicitly rejected before decoding. Upstream tests/corrupt.rs documents a rav1d 1.1.0 malformed AV1 path that can abort the process from an extern-C entry point; catch_unwind cannot protect that path. Do not enable untrusted AVIF import or decode cache bytes using this decoder without isolation or an upstream fix and renewed validation.

Color scope is SDR/sRGB with 4:4:4 output from ravif. Arbitrary ICC/HDR AVIF import is not implemented. 10/12-bit RGB output is expanded to the full u16 range by the wrapper.

MSRV is Rust 1.98, matching avif-decode 3.0.0. Retain the BSD license and this patch record when updating or redistributing.
