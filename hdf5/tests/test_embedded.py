"""Helper API contract when using the runtime-independent embedded filter."""
from pathlib import Path
import tempfile
import unittest

import h5py
import numpy as np

import radelta_hdf5


class EmbeddedTests(unittest.TestCase):
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
