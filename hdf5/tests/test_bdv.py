import itertools
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch
import xml.etree.ElementTree as ET

import h5py
import numpy as np
import tifffile

from radelta_hdf5 import FILTER_ID, configure_plugin, export_bdv
from radelta_hdf5.bdv import default_resolutions

if os.environ.get("RADELTA_HDF5_PLUGIN_PATH"):
    configure_plugin(os.environ["RADELTA_HDF5_PLUGIN_PATH"])


def reference_mean(data, factors):
    shape = tuple((n+f-1)//f for n, f in zip(data.shape, factors))
    result = np.empty(shape, np.uint16)
    for coord in np.ndindex(shape):
        values = [int(data[tuple(min(i*f+j, n-1) for i, f, j, n in
                                zip(coord, factors, offset, data.shape))])
                  for offset in itertools.product(*(range(f) for f in factors))]
        result[coord] = (sum(values)+len(values)//2)//len(values)
    return result


class BdvTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.input = self.root / "source.h5"
        self.output = self.root / "sample & test.xml"

    def tearDown(self):
        self.temp.cleanup()

    def source(self, shape=(2, 2, 5, 7, 9), axes="TCZYX"):
        data = (np.arange(np.prod(shape), dtype=np.uint32)*37 % 65536).reshape(shape).astype(np.uint16)
        with h5py.File(self.input, "w") as f:
            ds = f.create_dataset("images", data=data.astype(">u2"))
            ds.attrs["axes"] = axes
            ds.attrs["element_size_um"] = (2, .5, .25)
        return data

    def test_pyramid_unsigned_axes_metadata_and_regions(self):
        data = self.source()
        updates = []
        xml, h5 = export_bdv(self.input, self.output, chunks=(3, 4, 5),
                             resolutions=((1, 1, 1), (1, 2, 2), (2, 4, 4)),
                             channel_names=("green & yellow", "red <far>"),
                             progress=lambda done, total: updates.append((done, total)))
        root = ET.parse(xml).getroot()
        self.assertEqual(root.findtext("SequenceDescription/ImageLoader/hdf5"), h5.name)
        self.assertEqual(root.findtext("SequenceDescription/ViewSetups/ViewSetup/size"), "9 7 5")
        self.assertEqual(root.findtext("SequenceDescription/ViewSetups/ViewSetup/voxelSize/size"), "0.25 0.5 2.0")
        self.assertEqual(root.findtext("SequenceDescription/Timepoints/last"), "1")
        self.assertEqual(root.findtext("SequenceDescription/ViewSetups/ViewSetup/name"), "green & yellow")
        self.assertEqual(len(root.findall("ViewRegistrations/ViewRegistration")), 4)
        affine = list(map(float, root.findtext("ViewRegistrations/ViewRegistration/ViewTransform/affine").split()))
        np.testing.assert_array_equal(np.asarray(affine).reshape(3, 4),
                                      [[.25, 0, 0, 0], [0, .5, 0, 0], [0, 0, 2, 0]])
        with h5py.File(h5) as f:
            for t, c in itertools.product(range(2), range(2)):
                expected = data[t, c]
                for level, ratio in enumerate(((1, 1, 1), (1, 2, 2), (2, 2, 2))):
                    if level:
                        expected = reference_mean(expected, ratio)
                    ds = f[f"t{t:05d}/s{c:02d}/{level}/cells"]
                    self.assertEqual(ds.dtype, np.dtype("uint16"))
                    np.testing.assert_array_equal(ds[:], expected)
                    np.testing.assert_array_equal(ds[1:3, 1:3, 1:4], expected[1:3, 1:3, 1:4])
                    plist = ds.id.get_create_plist()
                    self.assertEqual(plist.get_filter(0)[0], FILTER_ID)
                    self.assertEqual(plist.get_filter(0)[1] & h5py.h5z.FLAG_OPTIONAL, 0)
                    self.assertEqual(ds.id.read_direct_chunk((0, 0, 0))[0], 0)
            np.testing.assert_array_equal(f["s00/resolutions"][:], [[1, 1, 1], [2, 2, 1], [4, 4, 2]])
            for level in range(3):
                np.testing.assert_array_equal(f["s00/subdivisions"][level],
                                              f[f"t00000/s00/{level}/cells"].chunks[::-1])
        self.assertEqual(updates[-1][0], updates[-1][1])
        self.assertEqual([x[0] for x in updates], list(range(1, len(updates)+1)))
        # The XML and HDF5 remain usable when moved as a pair.
        moved = self.root / "moved"
        moved.mkdir()
        xml.rename(moved / xml.name)
        h5.rename(moved / h5.name)
        reference = ET.parse(moved / xml.name).findtext("SequenceDescription/ImageLoader/hdf5")
        with h5py.File(moved / reference) as f:
            np.testing.assert_array_equal(f["t00001/s01/0/cells"][:], data[1, 1])

    def test_tiff_time_channels_and_ome_calibration(self):
        data = self.source((2, 3, 2, 7, 9), "TZCYX")
        path = self.root / "input.ome.tif"
        tifffile.imwrite(path, data, photometric="minisblack", ome=True,
                          metadata={"axes": "TZCYX", "PhysicalSizeX": .2,
                                    "PhysicalSizeY": .3, "PhysicalSizeZ": 1.5,
                                    "Channel": {"Name": ["A", "B"]}})
        for cache in (0, 1):
            xml, h5 = export_bdv(path, self.root / f"tiff{cache}.xml", chunks=(2, 4, 5),
                                 resolutions=((1, 1, 1),), tiff_cache_mib=cache)
            with h5py.File(h5) as f:
                for t, c in itertools.product(range(2), range(2)):
                    np.testing.assert_array_equal(f[f"t{t:05d}/s{c:02d}/0/cells"][:], data[t, :, c])
            root = ET.parse(xml)
            self.assertEqual(root.findtext("SequenceDescription/ViewSetups/ViewSetup/name"), "A")
            self.assertEqual(root.findtext("SequenceDescription/ViewSetups/ViewSetup/voxelSize/size"), "0.2 0.3 1.5")

    def test_two_dimensions_and_reordered_hdf5_axes(self):
        for shape, axes in (((7, 9), "YX"), ((2, 3, 2, 7, 9), "TZCYX")):
            data = self.source(shape, axes)
            xml, h5 = export_bdv(self.input, self.output, overwrite=True, resolutions=((1, 1, 1),))
            with h5py.File(h5) as f:
                expected = data[1, :, 1] if axes == "TZCYX" else data[None]
                path = "t00001/s01/0/cells" if axes == "TZCYX" else "t00000/s00/0/cells"
                np.testing.assert_array_equal(f[path][:], expected)

    def test_tiff_2d_metric_units_and_imagej_calibration(self):
        data = np.arange(7*9, dtype=np.uint16).reshape(7, 9)
        ome = self.root / "mixed-units.ome.tif"
        tifffile.imwrite(ome, data, ome=True, photometric="minisblack",
                          metadata={"axes": "YX", "PhysicalSizeY": 300,
                                    "PhysicalSizeYUnit": "nm", "PhysicalSizeX": .2,
                                    "PhysicalSizeXUnit": "µm"})
        ij = self.root / "imagej.tif"
        tifffile.imwrite(ij, np.stack([data, data]), imagej=True, resolution=(4, 2),
                          metadata={"axes": "ZYX", "spacing": 2, "unit": "um"})
        for path, expected in ((ome, [.2, .3, .2]), (ij, [.25, .5, 2])):
            xml, _ = export_bdv(path, self.output, overwrite=True)
            root = ET.parse(xml)
            spacing = root.findtext("SequenceDescription/ViewSetups/ViewSetup/voxelSize/size")
            np.testing.assert_allclose(list(map(float, spacing.split())), expected)
            self.assertEqual(root.findtext("SequenceDescription/ViewSetups/ViewSetup/voxelSize/unit"), "um")

    def test_lossy_quantised_once(self):
        self.source((5, 7, 9), "ZYX")
        _, h5 = export_bdv(self.input, self.output, resolutions=((1, 1, 1), (2, 2, 2)),
                            lossy=True, offset_adu=100, gain_e_per_adu=.46, noise_step=2)
        with h5py.File(h5) as f:
            base = f["t00000/s00/0/cells"]
            coarse = f["t00000/s00/1/cells"]
            self.assertTrue(base.attrs["radelta_lossy"])
            self.assertFalse(coarse.attrs["radelta_lossy"])
            np.testing.assert_array_equal(coarse[:], reference_mean(base[:], (2, 2, 2)))

    def test_publication_failures_and_overwrite(self):
        data = self.source((5, 7, 9), "ZYX")
        _, old_h5 = export_bdv(self.input, self.output, resolutions=((1, 1, 1),))
        old_xml = self.output.read_bytes()
        files = set(self.root.iterdir())
        with self.assertRaises(ValueError):
            export_bdv(self.input, self.output)
        for hook in ("radelta_hdf5.bdv._Hdf5Source.read", "radelta_hdf5.bdv.os.replace"):
            with patch(hook, side_effect=OSError("injected failure")):
                with self.assertRaises(OSError):
                    export_bdv(self.input, self.output, overwrite=True)
            self.assertEqual(self.output.read_bytes(), old_xml)
            self.assertEqual(set(self.root.iterdir()), files)
        _, new_h5 = export_bdv(self.input, self.output, overwrite=True)
        self.assertNotEqual(old_h5, new_h5)
        self.assertTrue(old_h5.exists())
        with h5py.File(old_h5) as f:
            np.testing.assert_array_equal(f["t00000/s00/0/cells"][:], data)

    def test_invalid_parameters_and_cli(self):
        self.source((5, 7, 9), "ZYX")
        for options in ({"chunks": (0, 1, 1)}, {"resolutions": ((2, 2, 2),)},
                        {"resolutions": ((1, 1, 1), (2, 2, 2), (3, 4, 4))},
                        {"voxel_size": (float("nan"), 1, 1)}, {"channel_names": ["A", "B"]},
                        {"tiff_cache_mib": -1}):
            with self.subTest(options=options), self.assertRaises(ValueError):
                export_bdv(self.input, self.output, **options)
            self.assertEqual(set(self.root.iterdir()), {self.input})
        environment = dict(os.environ)
        if os.environ.get("RADELTA_HDF5_PLUGIN_PATH"):
            environment["HDF5_PLUGIN_PATH"] = os.environ["RADELTA_HDF5_PLUGIN_PATH"]
        result = subprocess.run([sys.executable, "-m", "radelta_hdf5", "bdv",
                                 str(self.input), str(self.output), "--chunks", "3,4,5",
                                 "--resolutions", "1,1,1;2,2,2", "--voxel-size", "2,.5,.25"],
                                capture_output=True, text=True, env=environment)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("100.0%", result.stderr)
        self.assertIn("Wrote", result.stderr)

    def test_concurrent_publication_does_not_clobber_or_leave_hdf5(self):
        self.source((5, 7, 9), "ZYX")
        def racing_link(source, target):
            Path(target).write_bytes(b"concurrent writer")
            raise FileExistsError("concurrent writer won")
        with patch("radelta_hdf5.bdv.os.link", side_effect=racing_link):
            with self.assertRaises(FileExistsError):
                export_bdv(self.input, self.output)
        self.assertEqual(self.output.read_bytes(), b"concurrent writer")
        self.assertEqual(set(self.root.iterdir()), {self.input, self.output})

    def test_automatic_anisotropic_resolutions(self):
        levels = default_resolutions((100, 2048, 2048), (5, .5, .5))
        self.assertEqual(levels[1], (1, 2, 2))
        self.assertEqual(levels[-1], (1, 8, 8))
        self.assertEqual(default_resolutions((1, 7, 9), (1, 1, 1)), ((1, 1, 1),))


if __name__ == "__main__":
    unittest.main()
