# Read and write Radelta-compressed HDF5 files in Fiji

The Radelta ImageJ JAR bundles the native HDF5 filter and registers it
automatically in Fiji's HDF5 runtime. The complete
`radelta-imagej-<version>-fiji.zip` also includes the existing HDF5_Vibez
importer and its reader dependencies. Extract that ZIP, close Fiji and run
`python install.py /path/to/Fiji.app`. Full installation instructions are in
the complete bundle's `README.md`.

If HDF5_Vibez/JHDF5 is already installed, replace the Radelta ImageJ JAR and
restart Fiji. No `HDF5_PLUGIN_PATH` setting is needed. Open files through
**Plugins → Radelta → Open HDF5…**, or **File → Import → HDF5…** in Fiji.
For the `(186,613,860)` `/images` example, choose layout `zyx`.

Scripts can use the same integration without a graphical dialog:

```groovy
import org.radelta.imagej.RadeltaHdf5
def image = RadeltaHdf5.open('/path/to/test_radelta.h5', '/images', 'zyx')
image.show()
```

The importer supports uint16 data, both byte orders and ranks 2–5. Layouts
are `yx`, `zyx`, `czyx` and `tczyx`. It preserves channel/frame ordering and
reads physical spacing from the dataset's float `element_size_um` attribute
in `Z,Y,X` order, in micrometres. Without that attribute it uses unit spacing.
Lossy data displays its reconstructed pixels. The importer loads the selected
dataset into RAM; our `.rdlt` virtual-stack dialog does not apply to HDF5.

## Writing from Fiji

Save the current 8- or 16-bit grayscale image or hyperstack with
**Plugins → Radelta → Save as Radelta HDF5 (lossless)…** or **(lossy)…**.
The dialog chooses the dataset name (default `/images`), the Z/Y/X chunk
extents and, for lossy data, the camera offset, gain and noise step. 8-bit
images are stored as uint16. The file is written beside the target and then
renamed over it, so a failed save leaves any existing file unchanged. As with
`.rdlt` exports, a virtual stack cannot be saved over its own source file or
a hard link to it, and a plane that cannot be read aborts the save.

Datasets match the `radelta-hdf5` tools: uint16 in canonical `T,C,Z,Y,X`
order, starting at the first non-singleton axis (`yx`, `zyx`, `czyx` or
`tczyx`, which is also the layout to choose when importing). Chunks hold one
T/C index and default to at most 4 planes and 1024×1024 pixels, within 8 MiB.
Attributes record `axes`, `radelta_version`, `radelta_lossy`, the lossy
calibration and, when the image is calibrated in a length unit,
`element_size_um` (`Z,Y,X`, micrometres). Other ImageJ metadata, such as
slice labels and properties, is not stored; use `.rdlt` to keep it.

Scripts can save without dialogs:

```groovy
import org.radelta.imagej.RadeltaHdf5
RadeltaHdf5.save(image, '/path/to/output.h5', '/images')
RadeltaHdf5.saveLossy(image, '/path/to/lossy.h5', '/images', 100, 0.46, 2)
// Explicit Z/Y/X chunks; 0 keeps a default extent.
RadeltaHdf5.save(image, '/path/to/output.h5', '/images', false, 0, 1, 2, 8, 512, 512)
```

Macros use the keys `output`, `dataset`, `z`, `y` and `x` (and `offset`,
`gain`, `gain_0` for the gain units and `noise` for lossy saves).

Any HDF5 reader with a Radelta filter, including h5py and the `radelta-hdf5`
tools with the SDK filter, can read the result. Writing uses the same
embedded filter as reading: it makes no HDF5 API calls, so the plugin supplies
the chunk geometry and byte order with the other filter parameters.

Do not add the full SDK runtime to Fiji's library search paths. If an old
`HDF5_PLUGIN_PATH` points to that SDK filter or an older Radelta decoder,
remove it before starting Fiji; the plugin reports which filter is registered.

JHDF5 19.04.1 supports Windows x86-64, Linux x86-64 and both macOS
architectures. Linux ARM64 needs a compatible JHDF5 native build; that
platform's supplied JAR lacks one. The Radelta decoder and `.rdlt` support
are still built for all five platforms. Windows requires the Microsoft Visual
C++ 2015–2022 x64 runtime; its supported JHDF5 and decoder use the shared UCRT.
A Mac using Intel Java under Rosetta needs the x86-64 decoder, chosen
automatically from the Java architecture.

## Standalone embedded filter for other HDF5 applications

The separate `radelta-hdf5-fiji-<version>-<platform>-<arch>.zip` remains
available for native readers that need an explicit plugin directory. It
contains only the embedded filter and documentation, with no additional HDF5
runtime. Set `HDF5_PLUGIN_PATH` to its extracted `plugins` directory before
starting the reader. It decodes every Radelta dataset. It encodes only datasets
created with all 17 filter parameters (see [filter parameters](README.md#filter-parameters-and-id));
with the usual 0 or 11, writes fail, so use the full SDK filter for general
writing. Both builds have the private ID 65024; choose one Radelta filter
directory per reader.

## Integration tests

The build workflow tests the extracted standalone filter through HDF5
1.14.6 on all five platforms: it reads datasets written by the SDK filter, the
SDK filter reads datasets it writes with explicit parameters, and it rejects
writes without them. On the four platforms supported by JHDF5 it also starts
the ImageJ plugin's SciJava HDF5 service from its built JAR with
`HDF5_PLUGIN_PATH` unset. It checks exact unsigned pixels, lossless/lossy
reference images, both byte orders, ranks 2–5, padded edge chunks,
multi-channel/time chunks, hyperstack axis ordering, physical calibration
and chunk-crossing hyperslabs. The plugin then saves lossless and lossy
8/16-bit images of ranks 2–5 with edge chunks, nested groups and replacement;
each is checked through JHDF5 (pixels, chunks, all filter parameters and
attributes) and Fiji's importer, and the SDK filter re-reads it in a separate
native process. Linux ARM64's Java test is explicitly skipped.

To test a built JAR against installed HDF5 fixture tools:

```sh
python scripts/test_hdf5_fiji.py --platform linux --arch x86_64 \
  --prefix _deps/hdf5-install \
  --archive dist/radelta-hdf5-fiji-0.7.4-linux-x86_64.zip \
  --imagej-jar imagej-plugin/target/radelta-imagej-0.7.4.jar
```

Tests require JDK 17 and download SHA256-pinned Fiji/SciJava dependencies.
The release bundle keeps the unmodified third-party reader JARs separate,
with their licences and the HDF5 importer's complete source/build files.
