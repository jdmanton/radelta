# Install Radelta and HDF5 support together

Download `radelta-imagej-<version>-fiji.zip` from the release and extract it.
It contains the Radelta ImageJ JAR with native codec libraries and HDF5
decoders, plus the HDF5_Vibez importer and its reader dependencies.
Third-party JARs stay separate, with their licences and importer source.

Close Fiji, then run the included installer with Python 3.8 or later:

```powershell
python .\install.py 'C:\Fiji.app'
```

On Linux/macOS:

```sh
python3 install.py /path/to/Fiji.app
```

Choose the Fiji directory containing its `plugins` and `jars` folders. The
installer reuses reader libraries already installed, adds missing ones, and
disables duplicate Radelta JARs by retaining them with a `.jar.disabled` suffix.
It does not overwrite Fiji's ImageJ core. If an unsupported or duplicate HDF5 reader
is detected, update it using Fiji's updater before retrying.

Restart Fiji and drag a Radelta HDF5 file onto it, or use **File → Open…**;
see [opening in Fiji](../hdf5/FIJI.md). To browse every dataset in a file,
choose **Plugins → Radelta → Open HDF5…**, or the
**File → Import → HDF5…** command. Select your dataset and axis layout:
In the importer, choose **individual hyperstacks (custom layout)** to enter
the layout explicitly.

| Dataset dimensions | Layout |
| --- | --- |
| `Y,X` | `yx` |
| `Z,Y,X` | `zyx` |
| `C,Z,Y,X` | `czyx` |
| `T,C,Z,Y,X` | `tczyx` |

For the `(186,613,860)` `/images` example, use `zyx`. The plugin extracts the
appropriate decoder for Fiji's Java architecture and registers its directory
with JHDF5 automatically. **No HDF5_PLUGIN_PATH setting is needed.** Remove
any setting pointing at the full Radelta HDF5 SDK before starting
Fiji, especially if scripts initialise HDF5 before Fiji's startup services.

If the reader dependencies are already installed, copying only the released
`radelta-imagej-<version>.jar` into `plugins` is sufficient. Remove duplicate
Radelta JARs and restart Fiji. In ImageJ1 without SciJava startup services,
use the **Radelta → Open HDF5** command to initialise the decoder first.

Scripts can import without displaying an image:

```groovy
import org.radelta.imagej.RadeltaHdf5
def image = RadeltaHdf5.open('/path/to/image.h5', '/images', 'zyx')
image.show()
```

HDF5 import supports lossless and calibrated-lossy Radelta uint16 datasets,
either byte order and ranks 2–5. Pixel spacing comes from the dataset's float
`element_size_um` attribute in `Z,Y,X` order, in micrometres. HDF5_Vibez loads
the selected dataset into RAM; it does not provide our `.rdlt` virtual-stack
opening dialog. **Plugins → Radelta → Save as Radelta HDF5 (lossless)…** and
**(lossy)…** write Radelta HDF5 datasets from Fiji; see
[Writing from Fiji](../hdf5/FIJI.md#writing-from-fiji).

The release JAR contains both native libraries for all five Radelta platforms.
The supplied JHDF5 reader supports Windows x86-64, Linux x86-64 and macOS
Intel/Apple Silicon. **Linux ARM64 HDF5 import and export need a compatible
JHDF5 native build**; the supplied JAR lacks it. `.rdlt` support is available on that platform.
Windows requires the Microsoft Visual C++ 2015–2022 x64 runtime. Fiji on macOS
must use a decoder matching its Java architecture, including Rosetta sessions.

Headless CI starts the SciJava HDF5 service from the built plugin JAR with
`HDF5_PLUGIN_PATH` unset, then checks unsigned pixels, lossless/lossy results,
byte order, hyperstack axes/calibration and partial reads against reference
fixtures. Native-library staging requires exact archive paths and all five
platform pairs before constructing the universal release JAR.
