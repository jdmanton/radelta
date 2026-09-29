# Radelta native codec

This directory contains the Rust library and the TIFF-aware `radelta` command-line program.

Build and test with:

```bash
cargo test
cargo build --release
```

The main project documentation is:

- [README](../README.md)
- [CLI reference](../docs/CLI.md)
- [File format](../docs/FORMAT.md)
- [C API](../docs/C_API.md)

Multidimensional arrays use contiguous `T,C,Z,Y,X` order with X fastest.
