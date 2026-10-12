import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

import h5py
import numpy as np
import tifffile

from radelta_hdf5 import FILTER_ID, configure_plugin, create_dataset, default_chunks, filter_options

PLUGIN = os.environ.get("RADELTA_HDF5_PLUGIN_PATH")
if PLUGIN:
    configure_plugin(PLUGIN)


class FilterTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.path = self.root / "image.h5"

    def tearDown(self):
        self.temp.cleanup()

    def roundtrip(self, shape, chunks, dtype="<u2", **codec):
        rng = np.random.default_rng(2)
        data = rng.integers(0, 65536, shape, dtype=np.uint16).astype(dtype)
        with h5py.File(self.path, "w") as f:
            ds = create_dataset(f, "images", data=data, dtype=dtype, chunks=chunks, **codec)
            # No optional filter: errors must propagate to the writer.
            self.assertEqual(ds.id.get_create_plist().get_filter(0)[1] & h5py.h5z.FLAG_OPTIONAL, 0)
        with h5py.File(self.path) as f:
            ds = f["images"]
            np.testing.assert_array_equal(ds[:], data)
            selection = tuple(slice(1, n, 2) for n in shape)
            np.testing.assert_array_equal(ds[selection], data[selection])
            for origin in ds.iter_chunks():
                offset = tuple(s.start for s in origin)
                mask, payload = ds.id.read_direct_chunk(offset)
                self.assertEqual(mask & 1, 0)
                self.assertIn(payload[:4], (b"RDL1", b"RDM2"))
        return data

    def test_ranks_endianness_edges_contexts(self):
        for shape, chunks in (((13, 17), (7, 9)), ((5, 13, 17), (3, 7, 9)),
                              ((2, 5, 13, 17), (2, 3, 7, 9)),
                              ((2, 2, 5, 13, 17), (2, 2, 3, 7, 9))):
            for dtype in ("<u2", ">u2"):
                for context in ("mean", "mean-signs", "signed3", "signs"):
                    with self.subTest(shape=shape, dtype=dtype, context=context):
                        self.roundtrip(shape, chunks, dtype, context=context)

    def test_lossy_calibration(self):
        data = np.arange(4000, dtype=np.uint16).reshape(5, 20, 40)
        for dtype in ("<u2", ">u2"):
            with h5py.File(self.path, "w") as f:
                create_dataset(f, "images", data=data.astype(dtype), dtype=dtype, chunks=(3, 11, 17),
                               lossy=True, offset_adu=100, gain_e_per_adu=.46, noise_step=2)
            with h5py.File(self.path) as f:
                electrons = np.maximum((data.astype(float)-100)*.46, 0)
                q = np.floor(2*np.sqrt(electrons)/2+.5)
                expected = np.clip(np.floor(q*q/.46+100+.5), 0, 65535).astype(np.uint16)
                np.testing.assert_array_equal(f["images"][:], expected)
                self.assertTrue(f["images"].attrs["radelta_lossy"])
                mask, payload = f["images"].id.read_direct_chunk((0, 0, 0))
                self.assertEqual(mask, 0)
                self.assertIn(payload[:4], (b"RDLQ", b"RDQ2"))

    def test_partial_updates_and_append(self):
        with h5py.File(self.path, "w") as f:
            ds = create_dataset(f, "images", shape=(0, 1, 3, 13, 17),
                                chunks=(1, 1, 2, 7, 9), maxshape=(None, 1, 3, 13, 17))
            ds.resize(2, axis=0)
            ds[1, 0, 1, 3:11, 4:14] = 65535
        with h5py.File(self.path) as f:
            expected = np.zeros((2, 1, 3, 13, 17), np.uint16)
            expected[1, 0, 1, 3:11, 4:14] = 65535
            np.testing.assert_array_equal(f["images"][:], expected)

    def test_zero_chunk(self):
        with h5py.File(self.path, "w") as f:
            create_dataset(f, "images", data=np.zeros((3, 11, 13), np.uint16), chunks=(2, 7, 9))
        with h5py.File(self.path) as f:
            self.assertFalse(f["images"][:].any())

    def test_trailing_fletcher32_checksum(self):
        data = np.arange(3*11*13, dtype=np.uint16).reshape(3, 11, 13)
        with h5py.File(self.path, "w") as f:
            dcpl = h5py.h5p.create(h5py.h5p.DATASET_CREATE)
            dcpl.set_chunk((2, 7, 9))
            dcpl.set_filter(FILTER_ID, 0, filter_options())
            dcpl.set_fletcher32()
            space = h5py.h5s.create_simple(data.shape)
            handle = h5py.h5d.create(f.id, b"images", h5py.h5t.STD_U16LE, space, dcpl=dcpl)
            ds = h5py.Dataset(handle)
            ds[:] = data
        with h5py.File(self.path) as f:
            np.testing.assert_array_equal(f["images"][:], data)
        with h5py.File(self.path, "r+") as f:
            ds = f["images"]
            mask, payload = ds.id.read_direct_chunk((0, 0, 0))
            damaged = bytearray(payload)
            damaged[len(damaged)//2] ^= 1
            ds.id.write_direct_chunk((0, 0, 0), bytes(damaged), filter_mask=mask)
        with h5py.File(self.path) as f:
            with self.assertRaises(OSError):
                f["images"][0:1, 0:1, 0:1]

    def test_bad_dtype_and_options(self):
        with h5py.File(self.path, "w") as f:
            for dtype in (np.float32, np.int16, np.uint8):
                with self.assertRaises(ValueError):
                    create_dataset(f, "bad", shape=(10, 10), dtype=dtype)
            for opts in ({"scale_bits": 12}, {"context": "unknown"},
                         {"lossy": True, "gain_e_per_adu": 0}, {"noise_step": float("nan")}):
                with self.assertRaises(ValueError):
                    create_dataset(f, "bad", shape=(10, 10), **opts)
            for dtype in (np.int16, np.float32, np.uint8):
                with self.assertRaises(ValueError):
                    f.create_dataset("bad", shape=(10, 10), dtype=dtype,
                                     chunks=(5, 5), compression=FILTER_ID)
            with self.assertRaises(ValueError):
                f.create_dataset("bad", shape=(10, 10), dtype="u2", chunks=(5, 5),
                                 compression=FILTER_ID, compression_opts=(99,)*11)
            with self.assertRaises(ValueError):
                f.create_dataset("bad", shape=(10, 10), dtype="u2", chunks=(5, 5),
                                 compression=FILTER_ID, shuffle=True)

    def test_corrupt_chunks_and_wrong_geometry(self):
        self.roundtrip((5, 13, 17), (3, 7, 9))
        with h5py.File(self.path, "r+") as f:
            ds = f["images"]
            _, payload = ds.id.read_direct_chunk((0, 0, 0))
            ds.id.write_direct_chunk((0, 0, 0), payload[:20])
        with h5py.File(self.path) as f:
            with self.assertRaises(OSError):
                f["images"][0:1, 0:1, 0:1]
        # Valid Radelta payload, but encoded for another chunk shape.
        other = self.root / "other.h5"
        with h5py.File(other, "w") as f:
            ds = create_dataset(f, "images", data=np.ones((2, 5, 6), np.uint16), chunks=(2, 5, 6))
        with h5py.File(other) as f:
            _, payload = f["images"].id.read_direct_chunk((0, 0, 0))
        with h5py.File(self.path, "r+") as f:
            f["images"].id.write_direct_chunk((0, 0, 0), payload)
        with h5py.File(self.path) as f:
            with self.assertRaises(OSError):
                f["images"][0:1, 0:1, 0:1]

    def cli(self, *args, success=True):
        command = [sys.executable, "-m", "radelta_hdf5"]
        if PLUGIN:
            command.extend(["--plugin-path", PLUGIN])
        result = subprocess.run(command + list(map(str, args)), text=True, capture_output=True)
        self.assertEqual(result.returncode == 0, success, result.stderr)
        return result

    def test_tiff_cli_axis_reordering_and_repack(self):
        data = np.arange(2*3*4*13*17, dtype=np.uint16).reshape(2, 3, 4, 13, 17)
        source = self.root / "source.tif"
        target = self.root / "out.tif"
        repacked = self.root / "repacked.h5"
        tifffile.imwrite(source, data, photometric="minisblack", metadata={"axes": "TZCYX"})
        self.cli("encode", source, self.path, "--chunks", "1,1,2,7,9")
        expected = data.transpose(0, 2, 1, 3, 4)
        with h5py.File(self.path) as f:
            np.testing.assert_array_equal(f["images"][:], expected)
            self.assertEqual(f["images"].attrs["axes"], "TCZYX")
        self.cli("repack", self.path, repacked, "--chunks", "1,1,3,11,15")
        self.cli("decode", repacked, target)
        with tifffile.TiffFile(target) as tif:
            self.assertEqual(tif.series[0].axes, "TCZYX")
            np.testing.assert_array_equal(tif.asarray(), expected)

    def test_cli_plain_tiff_and_failure_cleanup(self):
        source = self.root / "plain.tif"
        data = np.arange(5*11*13, dtype=np.uint16).reshape(5, 11, 13)
        tifffile.imwrite(source, data, photometric="minisblack", metadata=None)
        self.cli("encode", source, self.path)
        with h5py.File(self.path) as f:
            np.testing.assert_array_equal(f["images"][:], data)
        before = self.path.read_bytes()
        self.cli("encode", source, self.path, success=False)
        self.assertEqual(before, self.path.read_bytes())
        self.cli("encode", source, self.path, "--overwrite", "--lossy", success=False)
        self.assertEqual(before, self.path.read_bytes())
        broken = self.root / "broken.h5"
        self.cli("encode", source, broken, "--chunks", "1,2", success=False)
        self.assertFalse(broken.exists())
        self.assertEqual(list(self.root.glob(".radelta-*")), [])

    def test_missing_plugin_in_fresh_process(self):
        self.roundtrip((3, 11, 13), (2, 7, 9))
        env = dict(os.environ, HDF5_PLUGIN_PATH=str(self.root / "no-plugins"))
        env.pop("RADELTA_HDF5_PLUGIN_PATH", None)
        result = subprocess.run([sys.executable, "-c",
            "import h5py,sys; f=h5py.File(sys.argv[1]); f['images'][:]", str(self.path)],
            env=env, capture_output=True)
        self.assertNotEqual(result.returncode, 0)

    def test_chunk_selection(self):
        c = default_chunks((10, 8, 200, 20000, 20000))
        self.assertEqual(c[:2], (1, 1))
        self.assertLessEqual(np.prod(c)*2, 8*1024**2)
        for opts in ({"gain_e_per_adu": -1}, {"scale_bits": 7}):
            with self.assertRaises(ValueError):
                filter_options(**opts)


if __name__ == "__main__":
    unittest.main()
