# Test fixtures

Small Radelta and TIFF files used by the Rust test suite to check decoding,
multidimensional handling and image input.

The Radelta fixtures use format version 1 with header and block CRC-32s, compact
probability models, block depth 2, the default `Mean` context and scale bits 10.
Pixel values are defined by `golden_rdl1_expected` and `golden_rdm2_expected`
in `src/codec/test_support.rs`.
`manifest.json` records their dimensions, options and SHA-256 hashes.

`planar_u16_lzw_tiles.tif` is a little-endian, unsigned 16-bit TIFF with two
pages, three planar samples per page, 19 × 17 pixels per sample and 16 × 16 LZW
tiles. Its right and bottom tiles contain padding. Pixel values are generated
independently of Radelta; `tests/tiled_lzw.rs` computes the expected values in
Rust and checks both encoding modes, streaming, TIFF export and truncated input.
The fixture can be generated with NumPy, tifffile and imagecodecs:

```python
import numpy as np
import tifffile

data = ((np.arange(2 * 3 * 17 * 19, dtype="u2") * 37) % 127).reshape(2, 3, 17, 19)
tifffile.imwrite("planar_u16_lzw_tiles.tif", data, planarconfig="separate",
                 tile=(16, 16), compression="lzw", photometric="minisblack",
                 byteorder="<", metadata=None)
```
