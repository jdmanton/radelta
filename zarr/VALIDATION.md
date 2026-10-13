# OME-Zarr integration validation

Base: public upstream tag v0.6.0, commit
`86bafb49b281eefec6c2fadbccf0d11927010de8`.

Tested locally on Linux x86-64, Python 3.12, NumPy 2.3.5, tifffile 2026.9.20,
with Zarr-Python 3.4.1 / numcodecs 0.17.0. The same 14 integration test methods
also passed with Zarr-Python 3.1.2 / numcodecs 0.16.3. The axes/endian/context
method contains 112 parameter combinations across both storage formats.

The suite covers lossless equality, calibrated lossy reconstruction, zero/fill
quantisation, omitted spatial/channel axes, C/F order, both byte orders,
checksum errors and dimension mismatch, corrupt-chunk isolation on partial
reads, resize/partial updates, v3 sharding, metadata-only images far beyond RAM,
XY mean pyramids with scale/origin transforms, TIFF axis order/calibration/export,
existing multiscales and labels, metadata/sidecar preservation and output protection.
Fresh Python readers successfully discover both registered codec entry points.

Native release ZIP creation and platform-specific Python wheel generation
were exercised. The wheel contains the native library and is tagged
`py3-none-linux_x86_64`, with `Root-Is-Purelib: false`. A separate installation
outside the checkout successfully loaded its own bundled native library and
passed both storage-format round trips and region reads. No HDF5 dependency
was loaded by these tests.

Python compilation, release metadata verification, Actionlint and git whitespace
checks passed. The new five-platform CI workflow builds/installs/tests native
release packages and uploads Actions artifacts; tagged native release builds
also run the OME-Zarr integration suite before publication. Windows, Linux ARM64
and macOS jobs were not executed locally.

The Rust/C bitstream and ABI are unchanged. Radelta Zarr chunk framing and codec
configuration are versioned separately at schema version 1. See README.md for
reader requirements, supported axes, calibration and conversion limitations.
