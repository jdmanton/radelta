# Radelta file format

Radelta stores unsigned 16-bit microscopy images using a square-root-domain predictive transform followed by static entropy coding. All integer fields are little-endian. Floating-point fields are little-endian IEEE-754 `f64`.

Multidimensional datasets use contiguous `T,C,Z,Y,X` order with X fastest. Prediction never crosses channel or time boundaries.

## Lossless transform

For each sample `x`:

```text
q = floor(sqrt(x))
r = x - q*q
```

For `uint16` data:

```text
0 <= q <= 255
0 <= r <= 2*q
```

Reconstruction is exact:

```text
x = q*q + r
```

The integer square root is computed exactly; floating-point rounding is not involved.

## Predicting q

Let `L`, `U`, and `Z` be the previously decoded `q` values immediately to the left,
above, and in the previous local Z plane. The default `Mean` context (mode `2`)
uses

```text
context = L + U + Z
```

Using the sum retains the one-third increments of the mean without
floating-point arithmetic. The optional `MeanSigns` context (mode `3`) further
conditions the model on the ordering of the left and upper neighbours:

```text
context = (L + U + Z) * 3 + compare(L, U)
```

Here `compare` is `0` for `<`, `1` for `=`, and `2` for `>`. `MeanSigns` uses
three times as many compact contexts as `Mean`, trading table size for finer
conditioning. On the first local Z plane, both modes substitute
`Z = floor((L + U) / 2)` so upper neighbours remain useful at block boundaries.

The alternative `Signs` (mode `1`) and `Signed3` (mode `0`) contexts use

```text
p = median(L,U,Z)
```

The `Signs` context maps each comparison to three classes (`<`, `=`, `>`):

```text
c1 = compare(L, U)
c2 = compare(Z, p)
context = (p * 3 + c1) * 3 + c2
```

The alternative `Signed3` context uses five classes for each integer difference:

```text
0       -> 0
+1      -> 1
>= +2   -> 2
-1      -> 3
<= -2   -> 4
```

and therefore uses

```text
c1 = category(L - U)
c2 = category(Z - p)
context = (p * 5 + c1) * 5 + c2
```

Let `B` be the number of compact contexts: `3 * q_alphabet` for `Mean`,
`9 * q_alphabet` for `MeanSigns` and `Signs`, and `25 * q_alphabet` for
`Signed3`. At `x=0`, use context `B + q_alphabet`; otherwise at `y=0`, use
`B + L`. `Signs` and `Signed3` also use `B + L` throughout the first local Z
plane. Each model has `B + q_alphabet + 1` contexts. Prediction never reads
across a block boundary.

## Coding r

The exact within-square remainder is modelled directly as

```text
P(r | q)
```

so each `q` value has its own remainder distribution. For `q=0`, the remainder is
necessarily zero, and its full-frequency rANS symbol leaves the state unchanged.
The encoder skips this operation; the decoder does so after checking that the
model is the zero-remainder identity.

## Probability models and rANS

Radelta learns static q and r histograms in a first pass. Each non-empty histogram is normalized to

```text
M = 1 << scale_bits
```

with a default `scale_bits` of 10. Supported values are 8–11.

Only nonzero frequencies are serialized. The symbols are then coded with byte-rANS using four interleaved states:

```text
LANES  = 4
RANS_L = 1 << 23
```

Independent Z blocks have independent rANS states and byte stacks, allowing block-level parallelism.

## Lossless single-volume file: RDL1

Magic: `RDL1`

Header:

| Field | Type |
| --- | --- |
| magic | 4 bytes |
| version | `u16` |
| flags | `u16` |
| X, Y, Z | `3 * u32` |
| block depth | `u32` |
| q maximum | `u16` |
| r maximum | `u16` |
| scale bits | `u8` |
| rANS lanes | `u8` |
| context mode | `u8` |
| reserved | `u8` |
| q-model length | `u32` |
| r-model length | `u32` |
| number of blocks | `u32` |
| q model | variable |
| r model | variable |

Each block stores:

| Field | Type |
| --- | --- |
| starting Z | `u32` |
| depth | `u32` |
| q byte-stack length | `u32` |
| r byte-stack length | `u32` |
| four q rANS states | `4 * u32` |
| four r rANS states | `4 * u32` |
| q byte stack | variable |
| r byte stack | variable |

## Calibrated-lossy single-volume file: RDLQ

Magic: `RDLQ`

The lossy transform first converts camera ADU values to electrons:

```text
electrons = max((ADU - offset_adu) * gain_e_per_adu, 0)
z         = 2 * sqrt(electrons)
q         = round(z / noise_step)
```

Only `q` is entropy-coded. Reconstruction is

```text
z         = q * noise_step
electrons = z*z / 4
ADU       = round(offset_adu + electrons / gain_e_per_adu)
```

with the result clamped to `uint16`.

The RDLQ header stores the XYZ dimensions, block/coder parameters, camera offset, gain, noise step, q model, and block count. Each block contains its Z position/depth, four q rANS states, and q byte stack.

## Multidimensional files: RDM2 and RDQ2

`RDM2` is lossless and `RDQ2` is calibrated lossy.

The header stores:

| Field | Type |
| --- | --- |
| magic | 4 bytes |
| version | `u16` |
| flags | `u16` |
| X, Y, Z, C, T | `5 * u32` |
| number of volumes (`T*C`) | `u32` |

Each `(T,C)` volume is then stored as an independent length-prefixed `RDL1` or `RDLQ` stream.

## Streaming files: RDS3 and RQS3

`RDS3` is lossless and `RQS3` is calibrated lossy. They are intended for large datasets that should not be held entirely in memory.

The header stores:

| Field | Type |
| --- | --- |
| magic | 4 bytes |
| version | `u16` |
| flags | `u16` |
| X, Y, Z, C, T | `5 * u32` |
| nominal chunk Z depth | `u32` |
| total number of chunks | `u64` |

Chunks are written in increasing `T,C,Z` order. Each chunk contains:

| Field | Type |
| --- | --- |
| T coordinate | `u32` |
| C coordinate | `u32` |
| starting Z | `u32` |
| depth | `u32` |
| payload length | `u64` |
| `RDL1` or `RDLQ` payload | variable |

The final chunk of a `(T,C)` volume may be shorter than the nominal chunk depth.

## Static model serialization

A probability model begins with:

| Field | Type |
| --- | --- |
| number of contexts | `u32` |
| alphabet size | `u16` |
| scale bits | `u8` |
| reserved (zero) | `u8` |

The remaining fields are canonical unsigned
base-128 varints: seven payload bits per byte, least-significant group first,
with bit 7 set when another byte follows. Values fit in `u32`; redundant leading
zero groups, overflow, and truncated encodings are rejected.

First write the number of nonempty contexts. For each nonempty context in
ascending order, write its index delta and number of nonzero symbols. For each
nonzero symbol in ascending order, write its index delta and normalized
frequency, except that the final frequency is inferred as
`(1 << scale_bits) - sum(previous frequencies)`. The first context index is
absolute; later context deltas must be positive. Symbol indices follow the same
rule, restarting at zero for each row. Empty contexts and zero-frequency symbols
are omitted.

Model bounds, index uniqueness, positive frequencies, and row totals are
validated when reading the model. All six container types use format version
`1`; multidimensional and streaming containers embed version `1` volume payloads.

## Opaque metadata (all containers)

Metadata is an optional byte payload owned by the application. Radelta does not
require XML, JSON, text, or a particular source format. The outer container's
`flags` bit `0x8000` indicates a metadata block after the last pixel payload.
Embedded volume/chunk offsets and pixel coding are unaffected. Metadata belongs
to the dataset; applications may encode per-plane records within that payload.

At the end of the file:

| Field | Type |
| --- | --- |
| metadata codec (`0` = raw, `1` = LZ4 block) | `u8` |
| uncompressed metadata length | `u64` |
| CRC-32/ISO-HDLC of the uncompressed bytes | `u32` |
| raw bytes or one LZ4 block (no size prefix/frame) | variable |
| metadata block length, including its 13-byte header | `u64` |
| footer magic | 8 bytes: `RDMETA01` |

LZ4 is used only when its result is smaller than the raw payload. Metadata has a
configurable uncompressed limit (1024 MiB by default); stored lengths, decoded lengths, and checksums are
validated when reading it. Empty metadata removes the block and clears the flag.
The limit is a process-wide allocation policy, not stored in the file or imposed
by the format. The footer allows readers to locate metadata without scanning or inflating image
payloads. Pixel-only memory decoders skip the block without inflating metadata;
metadata APIs and file readers validate its contents. Container versions remain
`1`.

### TIFF metadata payload

The TIFF adapter uses the opaque payload for a little-endian record beginning
with `RDTIFF01`, followed by a `u8` layout-valid flag, `u64` logical page count,
and one `u64` canonical TCZ plane index for each original TIFF page. A `u64`
physical IFD count follows, then a tag list per IFD. A contiguous ImageJ stack
may have just one physical IFD for many logical planes.

Each tag list starts with a `u64` count. Each tag stores its `u16` tag number,
`u16` TIFF type, `u64` byte length, value bytes, `u64` child-directory count,
and recursive tag lists for those children. Numeric values are little-endian;
ASCII, undefined, and byte values retain their original bytes. Directory links
are represented by child lists rather than original file offsets. Nesting is
limited to 16 levels. EXIF/GPS and typed IFD links are relocated on output.

Pixel storage tags (strip/tile offsets, byte counts, compression, etc.) are
regenerated by the TIFF encoder. Image pyramids/SubIFDs, thumbnails, and external
pixel payloads are outside this adapter's main-image conversion. Unknown private
values are preserved as opaque values; proprietary offsets hidden inside those
values cannot be interpreted or relocated.

### ImageJ metadata payload

The Fiji plugin writes an application-owned snapshot beginning with `RDIJ0001`.
This payload uses big-endian 32-bit nonnegative lengths: the original opaque
payload's byte length and bytes, then a field count and length-prefixed UTF-8
key/value pairs. No Java object serialization is used. The outer Radelta
metadata block compresses and checksums the complete snapshot.

Fields store calibration, title, per-plane labels, and supported image properties.
Property values carry a one-character type prefix: `T` string, `I` integer,
`L` long, `D` double, `F` float, `S` short, `B` byte, `Z` Boolean, or `Y`
Base64 bytes. Custom intensity calibration tables use Base64 big-endian floats.
The original payload is retained once, rather than nesting snapshots on each
save; current ImageJ values take precedence when reopening. Unknown source
metadata remains opaque. The CLI preserves an ImageJ snapshot in TIFF tag 65000;
it does not translate the snapshot fields into TIFF tags.
