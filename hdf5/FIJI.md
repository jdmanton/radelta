# Read Radelta-compressed HDF5 files in Fiji

The existing Radelta ImageJ JAR now bundles the native HDF5 decoder and
registers it automatically in Fiji's HDF5 runtime. The complete
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

This is read-only Radelta filter support. Use our existing HDF5 CLI/Python
tools or `h5repack` with the full SDK filter to encode Radelta HDF5 files.
Do not add the full SDK runtime to Fiji's library search paths. If an old
`HDF5_PLUGIN_PATH` points to that SDK filter, remove it before starting Fiji.

JHDF5 19.04.1 supports Windows x86-64, Linux x86-64 and both macOS
architectures. Linux ARM64 needs a compatible JHDF5 native build; that
platform's supplied JAR lacks one. The Radelta decoder and `.rdlt` support
are still built for all five platforms. Windows requires the Microsoft Visual
C++ 2015–2022 x64 runtime; its supported JHDF5 and decoder use the shared UCRT.
A Mac using Intel Java under Rosetta needs the x86-64 decoder, chosen
automatically from the Java architecture.

## Standalone decoder for other HDF5 applications

The separate `radelta-hdf5-fiji-<version>-<platform>-<arch>.zip` remains
available for native readers that need an explicit plugin directory. It
contains only the decoder and documentation, with no additional HDF5 runtime.
Set `HDF5_PLUGIN_PATH` to its extracted `plugins` directory before starting
the reader. The full read/write SDK filter has the same private ID, 65024;
choose one Radelta filter directory per reader.

## Integration tests

The build workflow tests the extracted standalone decoder through HDF5
1.14.6 on all five platforms. On the four platforms supported by JHDF5 it
also starts the ImageJ plugin's SciJava HDF5 service from its built JAR with
`HDF5_PLUGIN_PATH` unset. It checks exact unsigned pixels, lossless/lossy
reference images, both byte orders, ranks 2–5, padded edge chunks,
multi-channel/time chunks, hyperstack axis ordering, physical calibration
and chunk-crossing hyperslabs. The filter must advertise decoding only.
Linux ARM64's Java test is explicitly skipped.

To test a built JAR against installed HDF5 fixture tools:

```sh
python scripts/test_hdf5_fiji.py --platform linux --arch x86_64 \
  --prefix _deps/hdf5-install \
  --archive dist/radelta-hdf5-fiji-0.7.3-linux-x86_64.zip \
  --imagej-jar imagej-plugin/target/radelta-imagej-0.7.3.jar
```

Tests require JDK 17 and download SHA256-pinned Fiji/SciJava dependencies.
The release bundle keeps the unmodified third-party reader JARs separate,
with their licences and the HDF5 importer's complete source/build files.
