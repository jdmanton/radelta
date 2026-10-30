# Implementation architecture

Radelta has a Rust pixel codec, a file reader and streaming writer, and adapters
for TIFF, Python arrays, ImageJ/Fiji, HDF5 and OME-Zarr. The adapters supply
layout, sample-width and application metadata. The pixel codec operates on
unsigned 16-bit samples; adapters widen unsigned 8-bit input and preserve its
sample width for reconstruction.

See the [format specification](FORMAT.md) for serialized fields and the
[C API](C_API.md) for buffer ownership and calling conventions.

## Pixel codec

Each channel/timepoint volume is independent, and each volume contains
independent Z blocks. Spatial prediction stays within a block and never crosses
a channel or timepoint boundary. The default block depth is four Z planes.

Lossless encoding splits each value into `q = floor(sqrt(x))` and
`r = x - q*q`. A shared, immutable 65,536-entry lookup table supplies the exact
integer square root. Calibrated lossy encoding instead quantises
`2*sqrt(max((ADU-offset)*gain, 0))` using the supplied noise step. Decoding uses
the stored calibration to reconstruct the signal.

The default `Mean` context models `q` from causal neighbours using their exact
sum. `MeanSigns` also conditions on the ordering of the left and upper
neighbours. `Signs` and `Signed3` use median-based contexts. Lossless remainders
are conditioned on `q`. Integer histograms are combined across blocks to train
static probability models for the volume or streaming chunk.

Probability normalization uses integer arithmetic with deterministic rounding
and tie handling. Models track sorted nonempty contexts, and serialization and
decoder-table construction visit those contexts. Single-symbol rows assign
their entire probability total to that symbol. Reciprocal tables are shared
immutable data initialized on demand for the supported probability precisions.

Four interleaved rANS states encode each block. Specialized kernels implement
the default mean context and precision; general kernels handle the other
settings. Runs of zeros share histogram and context work only when the causal
neighbourhood satisfies the exact zero conditions. A decoder processes a group
as zeros only when every corresponding rANS slot selects zero.

The encoder compares the complete entropy-coded block cost with raw storage,
including model and framing overhead. Lossless raw blocks contain the input
samples; lossy raw blocks contain quantised symbols, so raw storage does not
bypass calibration. Header and block CRCs are checked during decoding.

## Parallel execution

Codec blocks, independent file-read units and queued streaming chunks use a
shared Rayon pool. Nested tasks cooperate within that pool. CLI `--threads`
selects a pool width; otherwise Rayon uses `RAYON_NUM_THREADS` or the available
CPUs.

Preprocessing partitions large arrays into tasks with a minimum grain of
65,536 voxels. Large lossless inputs can compute square-root symbols and their
maximum in one pass. Histogram work uses up to four batches per worker, bounded
by block count and a 64 MiB target for private histogram tables. Large models
use one table per worker, and a single worker uses one batch.

The file/cache mutex protects file reads and cache bookkeeping. Decoding takes
place outside it: a reader must not hold this mutex while waiting for Rayon
work that may itself need access to the same file.

## File indexing and plane access

Single-volume containers store one encoded volume. Multidimensional containers
index `(t,c)` volumes, and streaming containers index `(t,c,z0,depth)` chunks.
Opening a file reads record headers and validates bounds, dimensions, counts,
ordering and payload mode without decoding all pixels. Positioned reads are
used on Unix and Windows, with a seek/read implementation on other platforms.

The stored volume or chunk containing a requested plane is a decode unit.
`read_plane` copies a selected XY plane from that unit. `read_planes` groups
requested planes by unit, decodes missing units in parallel, and returns planes
in request order, including repeated coordinates. Batch waves account for both
compressed payloads and decoded pixels.

An adaptive LRU cache retains decoded units. A sequential pass keeps one
entry; same-Z channel switches or revisits enable multiple entries. Retained
bytes are counted by allocation capacity. The budget defaults to 2 GiB per
handle, grows on demand and permits at most 128 entries. Zero selects a
single-entry cache. One unit larger than the budget is allowed. Retention
budgets do not include codec scratch or caller-owned output.

Exporters can request owned decode-unit buffers directly, without retaining
them in the cache. The exporter controls each buffer's lifetime and accounts
for it until its last requested plane has been written.

## TIFF input

The CLI resolves OME or ImageJ layout metadata into canonical `T,C,Z,Y,X`
coordinates. Plain stacks default to one timepoint and one channel. Planar
multi-sample pages supply separate channel planes. Explicit overrides control
dimensions and page ordering; ambiguous mappings and unsupported sample layouts
produce errors.

Adjacent uncompressed grayscale strips are coalesced and read into their final
destination buffers, with byte swapping when required. This applies to unsigned
8-bit and 16-bit input and to selected samples in planar pages. Compressed or
tiled input uses the TIFF decoder. Full-width strips can decode into the final
plane; padded tiles use a reusable scratch buffer and copy visible rows.

Uncompressed contiguous ImageJ stacks are read as logical planes even when
their pixels follow a single physical directory. The input layout found during
inspection is reused by streaming encoding. TIFF metadata capture visits the
tags present in each directory, retaining supported nested and private values.

## Streaming encoding

A producer reads TIFF planes into reusable buffers while compression jobs run
in the shared pool. Completed chunks are written in TCZ order through a buffered
output handle. The planner selects chunk depth and a bounded queue of at most
eight pending chunks from the memory allowance, block depth and worker count.
Its working estimate reserves four raw chunk sizes per active slot. If the
allowance cannot support overlapping slots, encoding processes chunks serially.

Input buffers are recycled after compression. Pending work is joined on failure
and on writer close. The writer requires every declared plane before finish;
metadata is finalized and the output is published only after successful chunk
completion. Callers must check errors from both plane writes and finish.

## TIFF output

The exporter maps output pages to stored decode units, reads ahead while TIFF
directories are prepared, and borrows planes from owned decoded buffers. It
accounts for compressed payloads and reserves four times the decoded pixel
size for each resident unit. Interleaved page layouts retain a unit until its
last output plane.

If the planned resident units exceed the allowance, output uses cached plane
reads. That cache is limited to one quarter of the allowance, capped at 2 GiB.
ImageJ output keeps pixels contiguous. Directory prefixes up to 8 MiB are
serialized in memory and written as one buffered prefix; larger prefixes use a
file-backed serializer. Classic TIFF or BigTIFF is selected from the required
offsets and file size.

Metadata records retain source tags, page mappings and supported SubIFD
pyramids. Storage offsets and directory pointers are rebuilt for the output.
Pyramid pixels are compressed losslessly, including when the main image is
lossy. See [TIFF metadata handling](CLI.md#metadata-preservation) for limits and
unsupported layouts.

## Metadata and output publication

Metadata is an application-owned byte payload on the outer container. It uses
LZ4 when that is smaller than raw storage, with a length, checksum and footer.
The default uncompressed metadata limit is 1024 MiB and is independent of image
working buffers. Metadata APIs validate it; pixel-only memory decoders can skip
it without inflating it.

Rust output routines can write the header, borrowed pixel payload and metadata
using vectored I/O. Metadata replacement locates its block through the footer.
The Python array wrapper keeps the native allocation alive through metadata
attachment and releases it on success or failure.

File conversion stages output in a sibling temporary file. The CLI rejects
input/output aliases and publishes without replacing a destination unless
`--force` is supplied. No-replace publication uses a hard link on the output
filesystem. Failure leaves an existing destination intact and cleans up the
temporary output. See the [CLI reference](CLI.md) for filesystem requirements.

## Python and container adapters

Python memory decoding borrows immutable `bytes` for the duration of the native
call; mutable input is snapshotted. Full-XY file reads fill the final output via
batched native calls. XY crops use NumPy slice views and copy the selected
region into the result. Array metadata records shape, axes, dtype, byte order
and caller metadata.

HDF5 invokes the filter on individual chunks. The filter embeds the Rust codec
and checks decoded geometry against the dataset's chunk parameters. SDK builds
use HDF5 callbacks to derive those parameters. The embedded filter makes no
HDF5 calls; its caller supplies complete parameters. Python wheels register it
with h5py's HDF5 runtime, and Fiji registers it with JHDF5.

OME-Zarr wraps each chunk in an `RDZ1` envelope containing mode, byte count and
checksum. The shared adapter handles Zarr v2 and v3. Encoding checksums a borrowed
native output buffer before constructing the final frame. Decoding validates a
payload view and passes it to the native decoder. TIFF import assembles chunks
on a producer thread while a bounded set of workers compresses and writes them.

## Memory allowances

| Resource | Default | Scope |
| --- | --- | --- |
| Native CLI image working allowance | 2048 MiB | One encode/decode invocation |
| Native decoded retention cache | 2 GiB | One open file handle, at most 128 units |
| Native batch-read working allowance | 256 MiB | One batch, excluding retention and caller output |
| Metadata limit | 1024 MiB | Uncompressed metadata, configured process-wide |
| Python array-file write allowance | 256 MiB | Native streaming workspace; source array is separate |
| OME-Zarr TIFF page cache | 64 MiB | Decoded source pages during import |
| BigDataViewer TIFF page cache | 64 MiB | Decoded source pages during export |

These are working or retention allowances, not hard process-RSS limits. At least
one complete XY source plane and one stored decode unit may be required.
Probability models, metadata, TIFF directories, language runtimes and caller
arrays also occupy memory.
