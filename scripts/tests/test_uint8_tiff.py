"""Optional TIFF integration checks; requires numpy, tifffile and imagecodecs."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

TIFF_SKIP_REASON = None
try:
    import numpy as np
    import tifffile
    import imagecodecs  # Required for the LZW fixtures.
except ModuleNotFoundError as exc:
    TIFF_SKIP_REASON = f"TIFF integration requires numpy, tifffile and imagecodecs (missing {exc.name})"

CLI = os.environ.get("RADELTA_CLI", str(
    Path(__file__).resolve().parents[2] / "native" / "target" / "debug" /
    ("radelta.exe" if os.name == "nt" else "radelta")))


@unittest.skipIf(TIFF_SKIP_REASON is not None, TIFF_SKIP_REASON or "")
class Uint8TiffTests(unittest.TestCase):
    def run_cli(self, *args):
        # This integration matrix deliberately reuses its temporary outputs.
        result = subprocess.run([CLI, "--force", *map(str, args)], capture_output=True)
        self.assertEqual(result.returncode, 0, result.stderr.decode())

    @unittest.skipUnless(Path(CLI).is_file(), "Build the CLI or set RADELTA_CLI")
    def test_byte_depth_pixels_pyramids_and_lossy_range(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            data = np.arange(2*3*17*19, dtype=np.uint16).astype("u1").reshape(2, 3, 17, 19)
            for kind in ("plain", "imagej", "imagej_truncate", "ome", "planar", "planar_tiles", "pyramid"):
                for order in ("<", ">"):
                    source = root / "input.tif"
                    common = dict(photometric="minisblack", byteorder=order)
                    if kind == "plain":
                        tifffile.imwrite(source, data.reshape(6, 17, 19), metadata=None, **common)
                    elif kind.startswith("imagej"):
                        tifffile.imwrite(source, data, imagej=True, metadata={"axes": "ZCYX"},
                                         truncate=kind == "imagej_truncate", byteorder=order)
                    elif kind == "ome":
                        tifffile.imwrite(source, data, ome=True, metadata={"axes": "CZYX"}, **common)
                    elif kind.startswith("planar"):
                        tifffile.imwrite(source, data, planarconfig="separate", metadata=None,
                                         compression="lzw", tile=(16, 16) if kind == "planar_tiles" else None,
                                         **common)
                    else:
                        with tifffile.TiffWriter(source, byteorder=order) as out:
                            out.write(data.reshape(6, 17, 19), photometric="minisblack", subifds=1, metadata=None)
                            out.write(data.reshape(6, 17, 19)[:, ::2, ::2], photometric="minisblack",
                                      subfiletype=1, metadata=None)
                    for stream in (False, True):
                        with self.subTest(kind=kind, order=order, stream=stream):
                            encoded, restored = root / "image.rdlt", root / "restored.tif"
                            self.run_cli("encode", source, encoded, *(
                                ["--stream", "--memory-mib", "1"] if stream else []))
                            self.run_cli("decode", encoded, restored)
                            with tifffile.TiffFile(source) as a, tifffile.TiffFile(restored) as b:
                                self.assertEqual(b.pages[0].dtype, np.uint8)
                                # Inspect SubIFDs before asarray() can cache TiffFrame objects.
                                if kind == "pyramid":
                                    self.assertEqual(len(b.pages[0].pages), 1)
                                    for pa, pb in zip(a.pages, b.pages):
                                        np.testing.assert_array_equal(pa.pages[0].asarray(), pb.pages[0].asarray())
                                expected = a.asarray()
                                if kind.startswith("planar"):
                                    expected = expected.transpose(1, 0, 2, 3)  # Source ZCYX -> output CZYX.
                                np.testing.assert_array_equal(expected, b.asarray())
            values = np.arange(256, dtype="u1").reshape(16, 16)
            source, restored, encoded = root / "lossy.tif", root / "lossy-out.tif", root / "lossy.rdlt"
            tifffile.imwrite(source, values)
            expected = np.minimum(np.floor(np.sqrt(values.astype(float))+.5)**2, 255).astype("u1")
            for stream in (False, True):
                self.run_cli("encode-lossy", source, encoded, "--offset-adu", "0", "--gain-e-per-adu", "1",
                             *(["--stream"] if stream else []))
                self.run_cli("decode", encoded, restored)
                actual = tifffile.imread(restored)
                self.assertEqual(actual.dtype, np.uint8)
                np.testing.assert_array_equal(actual, expected)


if __name__ == "__main__":
    unittest.main()
