# Radelta C API

The public C interface is declared in `native/include/radelta.h`.

Shared-library names are:

```text
Windows   radelta.dll
Linux     libradelta.so
macOS     libradelta.dylib
```

## Status codes

```c
RADELTA_OK               =  0
RADELTA_BUFFER_TOO_SMALL =  1
RADELTA_INVALID_ARGUMENT = -1
RADELTA_CODEC_ERROR      = -2
RADELTA_IO_ERROR         = -3
```

Detailed file-API errors can be retrieved with `radelta_file_last_error`.

## In-memory lossless compression

`radelta_compress_u16` accepts contiguous `Z,Y,X` uint16 data. A convenient allocation pattern is to call it once with `output == NULL` to obtain the required encoded size, allocate a buffer, and call it again.

```c
size_t encoded_size = 0;
radelta_compress_u16(
    pixels, voxel_count, nx, ny, nz, NULL,
    NULL, 0, &encoded_size);

uint8_t *encoded = malloc(encoded_size);
radelta_compress_u16(
    pixels, voxel_count, nx, ny, nz, NULL,
    encoded, encoded_size, &encoded_size);
```

A NULL options pointer selects the defaults: block depth 4, `RADELTA_CONTEXT_MEAN`, and scale bits 10.
Because context value `0` is the explicit `RADELTA_CONTEXT_SIGNED3` mode, a zero-initialized options struct does not select the default context; pass NULL for all defaults or set `context_mode = RADELTA_CONTEXT_MEAN`.

## Decompression

`radelta_decompress_u16` can likewise be called first with `output == NULL` to query the output dimensions and required voxel count before allocating the destination buffer. When a sufficiently large output buffer is supplied, both lossless and lossy decoding write directly into that caller-owned buffer rather than constructing and copying an intermediate decoded volume.

For multidimensional data, use the `_nd` functions. Memory is contiguous `T,C,Z,Y,X` with X fastest. Lossless `RDM2` substreams are likewise decoded directly into their final TCZYX slices.

## Codec options

```c
typedef struct radelta_options {
    uint32_t block_depth;   /* 0 => 4 */
    uint32_t context_mode;  /* 0 Signed3, 1 Signs, 2 Mean (default), 3 MeanSigns */
    uint32_t scale_bits;    /* 0 => 10; supported 8..11 */
} radelta_options;
```

`RADELTA_CONTEXT_MEAN` uses the sum of causal neighbours and a compact table.
`RADELTA_CONTEXT_MEAN_SIGNS` additionally conditions on the left/upper ordering,
using larger tables that can improve compression ratio at a speed cost. Both
mean modes use left and upper neighbours on the first plane of each block.

Lossy compression additionally uses:

```c
double offset_adu;
double gain_e_per_adu;
double noise_step;
```

## Random-access file reader

The opaque `radelta_file_handle` API opens Radelta files and provides random `(t,c,z)` plane access.

Typical use:

```text
radelta_file_open
radelta_file_get_info
radelta_file_read_plane_u16
radelta_file_close
```

For streaming files, only the chunk containing the requested plane is decoded and cached.

Format identifiers returned by `radelta_file_info.format` are:

```text
1 RDL1
2 RDLQ
3 RDM2
4 RDQ2
5 RDS3
6 RQS3
```

Flags are bitwise:

```text
1 lossy
2 multidimensional
4 streaming
```

## Streaming writer

The writer API emits `RDS3` or `RQS3` files without holding the whole dataset in RAM. Planes are supplied sequentially in increasing `T,C,Z` order.

```text
radelta_writer_create_lossless_u16
  or radelta_writer_create_lossy_u16
radelta_writer_write_plane_u16
radelta_writer_finish
radelta_writer_close
```

`radelta_writer_finish` fails if not all expected planes have been supplied. `radelta_writer_close` releases the writer resources.

## Opaque metadata

Metadata is an arbitrary byte sequence, compressed losslessly with LZ4 when
beneficial, with a raw fallback and checksum. The default metadata limit is
1024 MiB (1,073,741,824 bytes).
No text encoding or application schema is imposed.

- `radelta_set_metadata` produces an encoded image with metadata attached or
  replaced, without recompressing pixels. It accepts the existing encoded bytes
  and metadata bytes; use the normal NULL-output size query before allocating
  the result. Zero metadata length removes it.
- `radelta_get_metadata` extracts metadata from an encoded buffer. NULL output
  queries the required byte count; absent metadata returns OK with size zero.
- `radelta_writer_set_metadata` copies metadata into a file writer before
  `radelta_writer_finish`. Repeated calls replace it; zero length clears it.
- `radelta_file_get_metadata` reads metadata from an open file handle using the
  same size-query pattern.

The pixel APIs decode files containing metadata normally. Metadata is stored
once on the outer dataset container, separate from per-volume/chunk pixels.

Rust callers can use `set_metadata(&mut encoded, bytes)` and
`read_metadata(&encoded)`, or `set_file_metadata(path, bytes)` and
`read_file_metadata(path)`. File operations read/write only the metadata and
container flags. `RadeltaFileHandle::open`, `metadata`, `info`, and `read_plane`
provide an indexed Rust file reader as well.

### Configuring the metadata limit

Set the process-wide limit before starting I/O:

```c
radelta_set_metadata_limit((size_t)2048 * 1024 * 1024); /* 2048 MiB */
size_t current_limit = radelta_get_metadata_limit();
```

Rust exposes `set_metadata_limit(bytes)`, `metadata_limit()`, and
`DEFAULT_METADATA_LIMIT_BYTES` (1024 MiB). Zero permits only empty metadata.
The setting applies to memory APIs, file opening/metadata operations, and
streaming writers. Existing readers retain metadata they have already loaded;
writers check the current limit again at finish. Configure once before concurrent
operations rather than changing policy while I/O is underway.

This limits uncompressed metadata size, not total working memory: compression
and decoding also need input/output buffers. Platform allocation limits still
apply. The setting does not change the on-disk format.
