from pathlib import Path
import tempfile
import unittest
from unittest import mock

import h5py
import numpy as np
import radelta
import radelta_hdf5


class Hdf5ApiTests(unittest.TestCase):
    def test_file_layout_metadata_and_slices(self):
        with tempfile.TemporaryDirectory() as d:
            for axes, shape in (("yx", (7, 9)), ("tyx", (2, 7, 9)),
                                ("tczyx", (2, 2, 3, 7, 9)), ("xzy", (9, 3, 7))):
                image = np.arange(np.prod(shape), dtype=np.uint16).reshape(shape).astype(">u2")
                path = Path(d)/(axes+".h5")
                radelta.write(path, image, axes=axes, metadata=b"metadata", dataset="/group/pixels")
                with radelta.open(path, dataset="/group/pixels") as source:
                    self.assertEqual(source.axes, axes)
                    self.assertEqual(source.metadata, b"metadata")
                    self.assertEqual(source.dtype, image.dtype)
                    np.testing.assert_array_equal(source[...], image)
                    np.testing.assert_array_equal(source[-1, ::-1, ...], image[-1, ::-1, ...])
                with h5py.File(path) as f:
                    ds = f["group/pixels"]
                    self.assertEqual(ds.id.get_create_plist().get_filter(0)[1] & h5py.h5z.FLAG_OPTIONAL, 0)

    def test_ordinary_hdf5_without_python_layout(self):
        with tempfile.TemporaryDirectory() as d:
            image = np.arange(3*7*9, dtype=np.uint16).reshape(3, 7, 9)
            path = Path(d)/"ordinary.h5"
            with h5py.File(path, "w") as f:
                radelta_hdf5.create_dataset(f, "images", data=image)
            np.testing.assert_array_equal(radelta.read(path), image)

    def test_partial_reads_select_only_requested_pixels(self):
        with tempfile.TemporaryDirectory() as d:
            image = np.arange(3*32*40, dtype=np.uint16).reshape(3, 32, 40)
            path = Path(d)/"regions.h5"
            radelta.write(path, image, chunks=(1, 8, 8))
            original = h5py.Dataset.__getitem__
            selections = []

            def record(dataset, key):
                selections.append(key)
                return original(dataset, key)

            with radelta.open(path) as source, mock.patch.object(h5py.Dataset, "__getitem__", record):
                np.testing.assert_array_equal(source[1, 19:4:-3, 30:3:-4], image[1, 19:4:-3, 30:3:-4])
                self.assertEqual(selections, [(1, slice(7, 20, 3), slice(6, 31, 4))])
                np.testing.assert_array_equal(source[:, :0], image[:, :0])

    def test_failed_replacement_preserves_destination(self):
        with tempfile.TemporaryDirectory() as d:
            image = np.arange(63, dtype=np.uint16).reshape(7, 9)
            path = Path(d)/"test.h5"
            radelta.write(path, image)
            original = path.read_bytes()
            with self.assertRaises(ValueError):
                radelta.write(path, image, overwrite=True, chunks=(1,))
            self.assertEqual(path.read_bytes(), original)
            self.assertEqual(list(Path(d).glob(".radelta-*")), [])


if __name__ == "__main__":
    unittest.main()
