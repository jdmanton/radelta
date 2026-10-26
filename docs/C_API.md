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

For multiple planes, use:

```c
radelta_file_read_planes_u16(handle, count, tcz, output, output_capacity_voxels);
```

`tcz` contains `3 * count` unsigned 32-bit coordinates in the order
`[t0,c0,z0,t1,c1,z1,...]`. Output contains `count` consecutive `nx * ny` planes
in that same order; repeated coordinates are supported. Missing volumes/chunks
decode once per call, in parallel using the shared Rayon pool. Rust callers can
use `RadeltaFileHandle::read_planes(&[(t,c,z), ...], &mut pixels)`.

Each call admits up to 256 MiB of additional decoded pixels plus compressed
payloads, or one larger unit. Retained cache data, codec scratch and caller output
are separate. Empty requests succeed and allow null coordinate/output pointers.
Invalid coordinates or insufficient capacity leave output untouched; decode/I/O
errors can leave partial output. The handle and input/output buffers must not
overlap, and the handle must remain open until the call returns.

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

## Decoded data cache

The native file reader retains recently decoded volumes/chunks in an LRU cache,
so channel-interleaved reads do not repeatedly decode the same data. It keeps a
single entry during a first sequential pass and enables multiple entries when
it detects interleaving or revisiting previously read data. Its default
decoded-data budget is 2 GiB per open handle, grown on demand, with at most
128 entries. Call
`radelta_file_set_cache_limit(handle, max_bytes)` to change this budget. Zero
restores the original single-entry cache. One oversized entry is always allowed;
this is a retention budget, not a cap on the decoder's total working memory.
Rust callers can use `RadeltaFileHandle::set_cache_limit(bytes)`.

Rust exporters can query `decode_unit_info(t,c,z)` for a stored unit's key,
starting Z, depth and compressed size, then call `decode_unit(t,c,z)` to obtain
owned Z,Y,X pixels without inserting them into the retention cache. Keys are
local to one open handle. Concurrent calls share the existing Rayon pool;
the handle lock covers payload reads, not codec work. Callers control the
number and lifetime of these owned buffers. The C ABI is unchanged.

## Streaming writer

The writer API emits `RDS3` or `RQS3` files without holding the whole dataset in RAM. Planes are supplied sequentially in increasing `T,C,Z` order.

```text
radelta_writer_create_lossless_u16
  or radelta_writer_create_lossy_u16
radelta_writer_write_plane_u16
radelta_writer_finish
radelta_writer_close
```

The streaming writer overlaps feeding planes with background compression and
ordered output, using the shared Rayon pool and a bounded working set selected
from `memory_mib`. Plane data are copied before a write call returns. A codec
error can surface on a later write or at finish; always check finish. Errors
from queued compression or output leave the writer failed and prevent publish.
Close joins every queued task before removing unfinished output.

Rust callers can use `RadeltaStreamWriter::create_lossless` or `create_lossy`,
then `write_plane`, `set_metadata` and `finish`. `flush_pending` waits for
submitted chunks without publishing or discarding a partially filled chunk.

`radelta_writer_finish` fails if not all expected planes have been supplied.
Writers stage pixels and metadata in a sibling temporary file; the destination
is created or replaced only when finish succeeds. Closing an unfinished writer
discards the temporary file and preserves any previous destination.
`radelta_writer_close` releases the writer resources. The destination directory
must have space for the complete temporary output alongside an existing file.

## Opaque metadata

Metadata is an arbitrary byte sequence, compressed losslessly with LZ4 when
beneficial, with a raw fallback and checksum. The default metadata limit is
1024 MiB (1,073,741,824 bytes).
No text encoding or application schema is imposed.

- `radelta_set_metadata` produces an encoded image with metadata attached or
  replaced, without recompressing pixels. It accepts the existing encoded bytes
  and metadata bytes; use the normal NULL-output size query before allocating
  the result. Zero metadata length removes it. Size queries do not copy the
  encoded image; an output call copies the pixel payload directly into the
  caller's buffer once.
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

To save an already encoded buffer without growing or copying that buffer to
attach metadata, Rust callers can use
`write_encoded_with_metadata(&mut writer, &encoded, bytes)`. It writes the
updated header, borrowed pixel payload and metadata as vectored I/O where the
writer supports it, and returns the total bytes written. Flush and close the
writer before publishing the destination. The supplied encoded buffer is
unchanged; passing empty metadata removes any existing metadata in the output.

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

## Allocating encoding

`radelta_compress_u16_nd_alloc` encodes once and returns an owned byte buffer.
Pass both options pointers as NULL for lossless defaults, or exactly one of
`radelta_options` and `radelta_lossy_options`. On success release the unchanged
pointer/size pair using `radelta_buffer_free`, never `free`. On failure the
output pointer and size are cleared. NULL buffer release is a no-op.

The optional [HDF5 filter](../hdf5/README.md) uses this API to avoid encoding
each chunk twice merely to determine the output size.
