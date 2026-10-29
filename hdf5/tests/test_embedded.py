"""Helper API contract when using the runtime-independent embedded filter."""
from pathlib import Path
import tempfile
import unittest

import h5py
import numpy as np

import radelta_hdf5


class EmbeddedTests(unittest.TestCase):
    def test_uint8_lossless_and_lossy(self):
        image = np.arange(512, dtype=np.uint16).astype(np.uint8).reshape(2, 16, 16)
        with tempfile.TemporaryDirectory() as d:
            path = Path(d) / "bytes.h5"
            for lossy in (False, True):
                with h5py.File(path, "w") as f:
                    radelta_hdf5.create_dataset(f, "data", data=image, chunks=(1, 7, 9), lossy=lossy)
                expected = np.minimum(np.floor(np.sqrt(image.astype(float))+.5)**2, 255).astype(np.uint8) if lossy else image
                with h5py.File(path) as f:
                    self.assertEqual(f["data"].dtype, np.uint8)
                    np.testing.assert_array_equal(f["data"][:], expected)

    def test_raw_fallback_retains_filter_and_pixel_semantics(self):
        rng = np.random.default_rng(78)
        with tempfile.TemporaryDirectory() as d:
            for endian in ("<u2", ">u2"):
                for lossy in (False, True):
                    q = rng.integers(0, 256, (3, 7, 9), dtype=np.uint16)
                    image = (q*q+q//2 if lossy else
                             rng.integers(0, 65536, q.shape, dtype=np.uint16)).astype(endian)
                    with h5py.File(Path(d)/"raw.h5", "w") as f:
                        ds = radelta_hdf5.create_dataset(f, "data", data=image,
                                dtype=endian, chunks=image.shape, lossy=lossy)
                        ds.id.flush()
                        mask, payload = ds.id.read_direct_chunk((0, 0, 0))
                        self.assertEqual(mask, 0)
                        self.assertEqual(payload[:4], b"RDQ2" if lossy else b"RDM2")
                        # ND container header and one volume-length prefix.
                        payload = payload[40:]
                        self.assertEqual(int.from_bytes(payload[4:6], "little"), 1)
                        # A single raw block, no model tables or rANS states.
                        self.assertEqual(len(payload), image.size*(1 if lossy else 2)
                                         + (68 if lossy else 48) + 20)
                    with h5py.File(Path(d)/"raw.h5") as f:
                        np.testing.assert_array_equal(f["data"][:], q*q if lossy else image)

    def test_explicit_geometry_and_mandatory_filter(self):
        with tempfile.TemporaryDirectory() as d, h5py.File(Path(d)/"test.h5", "w") as f:
            image = np.arange(3*7*9, dtype=np.uint16).reshape(3, 7, 9).astype(">u2")
            ds = radelta_hdf5.create_dataset(f, "data", data=image, dtype=image.dtype, chunks=(2, 4, 5))
            identifier, flags, params, _ = ds.id.get_create_plist().get_filter(0)
            self.assertEqual(identifier, 65024)
            self.assertEqual(flags & h5py.h5z.FLAG_OPTIONAL, 0)
            # Older h5py get_filter() exposes only a 16-word diagnostic
            # buffer. Check geometry here and byte order via decoded pixels.
            self.assertEqual(params[11:16], (5, 4, 2, 1, 1))
            ds.id.close()
            np.testing.assert_array_equal(f["data"][:], image)

    def test_invalid_pixel_data_is_rejected_by_helper(self):
        with tempfile.TemporaryDirectory() as d, h5py.File(Path(d)/"test.h5", "w") as f:
            with self.assertRaises(ValueError):
                radelta_hdf5.create_dataset(f, "bad", data=np.zeros((3, 5), np.float32))
            self.assertNotIn("bad", f)


if __name__ == "__main__":
    unittest.main()
