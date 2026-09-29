# Test fixtures

Small Radelta files used by the Rust test suite to check decoding and multidimensional handling.

Both fixtures use format version 1, compact probability models, block depth 2,
the default `Mean` context and scale bits 10. Pixel values are defined by
`golden_rdl1_expected` and `golden_rdm2_expected` in `src/lib.rs`.
`manifest.json` records their dimensions, options and SHA-256 hashes.
