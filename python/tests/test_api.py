import ctypes as ct
import itertools
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import numpy as np

import radelta
from radelta._native import U8, library, pixel_pointer


class ArrayTests(unittest.TestCase):
    def test_uint8_arrays_streaming_and_lossy_saturation(self):
        # Include every byte, unusual axes and negative strides.
        image = np.arange(512, dtype=np.uint16).astype(np.uint8).reshape(2, 2, 2, 8, 8)
        image = image.transpose(4, 2, 0, 1, 3)[::-1]
        for lossy in (False, True):
            options = dict(lossy=True, offset_adu=0, gain_e_per_adu=1) if lossy else {}
            expected = np.minimum(np.floor(np.sqrt(image.astype(float))+.5)**2, 255).astype(np.uint8) if lossy else image
            decoded = radelta.decompress(radelta.compress(image, axes="xztcy", **options))
            self.assertEqual(decoded.dtype, np.uint8)
            np.testing.assert_array_equal(decoded, expected)
            with tempfile.TemporaryDirectory() as d:
                path = Path(d) / "bytes.rdlt"
                radelta.write(path, image, axes="xztcy", memory_mib=1, **options)
                decoded = radelta.read(path)
                self.assertEqual(decoded.dtype, np.uint8)
                np.testing.assert_array_equal(decoded, expected)
                with radelta.open(path) as dataset:
                    self.assertEqual(dataset.dtype, np.uint8)
                    np.testing.assert_array_equal(dataset[::-2, :, 1], expected[::-2, :, 1])

    def test_decompress_accepts_immutable_mutable_and_strided_buffers(self):
        image = np.arange(105, dtype=np.uint16).reshape(3, 5, 7) * 317
        encoded = radelta.compress(image)
        strided = memoryview(bytearray(2 * len(encoded)))[::2]
        strided[:] = encoded
        for source in (encoded, bytearray(encoded), memoryview(encoded), strided):
            before = bytes(source)
            np.testing.assert_array_equal(radelta.decompress(source), image)
            self.assertEqual(bytes(source), before)
        for source in (b"", bytearray(), memoryview(b"")):
            with self.assertRaises(ValueError):
                radelta.decompress(source)

    def test_metadata_attachment_keeps_native_buffer_alive_and_frees_original_size(self):
        dll = library()
        allocated, freed = [], []

        class Binding:
            def radelta_compress_u16_nd_alloc(self, *args):
                status = dll.radelta_compress_u16_nd_alloc(*args)
                allocated.append(ct.cast(args[-1], ct.POINTER(ct.c_size_t)).contents.value)
                return status

            def radelta_set_metadata(self, *args):
                return dll.radelta_set_metadata(*args)

            def radelta_buffer_free(self, pointer, size):
                freed.append(size)
                dll.radelta_buffer_free(pointer, size)

        image = np.arange(35, dtype=np.uint16).reshape(5, 7)
        with patch("radelta._array.library", return_value=Binding()):
            encoded = radelta.compress(image, metadata=b"calibration" * 100)
        np.testing.assert_array_equal(radelta.decompress(encoded), image)
        self.assertEqual(freed, allocated)
        self.assertNotEqual(freed[0], len(encoded))

        with patch("radelta._array.library", return_value=Binding()), \
                patch("radelta._array.pack_layout", side_effect=ValueError("injected failure")):
            with self.assertRaisesRegex(ValueError, "injected failure"):
                radelta.compress(image)
        self.assertEqual(len(freed), 2)
        self.assertEqual(freed, allocated)

    def test_layout_strides_endianness_and_contexts(self):
        for axes in ("yx", "zyx", "tyx", "tzyx", "tczyx", "xztcy"):
            shape = tuple(dict(t=2, c=2, z=3, y=5, x=7)[a] for a in axes)
            original = (np.arange(np.prod(shape), dtype=np.uint32)*37 % 65536).reshape(shape)
            for dtype, context in itertools.product(("<u2", ">u2"), ("mean", "mean-signs", "signed3", "signs")):
                image = original.astype(dtype)[tuple(slice(None, None, -1) for _ in axes)]
                with self.subTest(axes=axes, dtype=dtype, context=context):
                    encoded = radelta.compress(image, axes=axes, context=context)
                    decoded = radelta.decompress(encoded)
                    self.assertIn(encoded[:4], (b"RDL1", b"RDM2"))
                    self.assertEqual(decoded.shape, image.shape)
                    self.assertEqual(decoded.dtype, image.dtype)
                    np.testing.assert_array_equal(decoded, image)

    def test_lossy_requires_calibration_and_matches_native_transform(self):
        data = np.arange(7*9, dtype=np.uint16).reshape(7, 9)*71
        with self.assertRaises(ValueError):
            radelta.compress(data, lossy=True)
        q = np.floor(np.sqrt(np.maximum((data.astype(float)-100)*.46, 0))+.5)
        expected = np.clip(np.floor(q*q/.46+100+.5), 0, 65535).astype(np.uint16)
        encoded = radelta.compress(data, lossy=True, offset_adu=100, gain_e_per_adu=.46)
        np.testing.assert_array_equal(radelta.decompress(encoded), expected)

    def test_input_validation_output_limit_and_corruption(self):
        image = np.arange(63, dtype=np.uint16).reshape(7, 9)
        for data, options in ((image.astype(np.float32), {}), (image, {"axes": "qq"}),
                              (image, {"axes": ""}), (image, {"axes": 12}),
                              (image, {"scale_bits": 7}), (image, {"context": "unknown"}),
                              (image, {"block_depth": -1}), (image, {"gain_e_per_adu": 0})):
            with self.subTest(options=options), self.assertRaises(ValueError):
                radelta.compress(data, **options)
        encoded = radelta.compress(image)
        with self.assertRaises(ValueError):
            radelta.decompress(encoded, max_output_bytes=image.nbytes-1)
        for data in (b"", encoded[:20], encoded[:-1], encoded[:40]+b"corrupt"+encoded[47:]):
            with self.assertRaises(ValueError):
                radelta.decompress(data)
        with self.assertRaises(TypeError):
            radelta.decompress(123)

    def test_existing_native_stream_without_python_metadata(self):
        image = np.arange(3*7*9, dtype=np.uint16).reshape(3, 7, 9)
        dll, output, size = library(), U8(), ct.c_size_t()
        self.assertEqual(dll.radelta_compress_u16_nd_alloc(pixel_pointer(image), image.size,
                         9, 7, 3, 1, 1, None, None, ct.byref(output), ct.byref(size)), 0)
        try:
            encoded = ct.string_at(output, size.value)
        finally:
            dll.radelta_buffer_free(output, size.value)
        np.testing.assert_array_equal(radelta.decompress(encoded), image)

    def test_core_import_does_not_load_optional_dependencies(self):
        code = "import sys,radelta; assert 'h5py' not in sys.modules; assert 'zarr' not in sys.modules"
        subprocess.run([sys.executable, "-c", code], check=True)

    def test_unaligned_arrays(self):
        image = np.ndarray((7, 9), dtype=np.uint16, buffer=bytearray(127), offset=1)
        image[:] = np.arange(63).reshape(7, 9)
        self.assertFalse(image.flags.aligned)
        np.testing.assert_array_equal(radelta.decompress(radelta.compress(image)), image)
        with tempfile.TemporaryDirectory() as d:
            path = Path(d)/"unaligned.rdlt"
            radelta.write(path, image)
            np.testing.assert_array_equal(radelta.read(path), image)


class FileTests(unittest.TestCase):
    def test_batched_plane_boundaries_and_reverse_tcz_selections(self):
        image = (np.arange(3 * 3 * 17 * 7 * 9, dtype=np.uint16)
                 .reshape(3, 3, 17, 7, 9) * 13)
        path = self.root / "batches.rdlt"
        radelta.write(path, image, axes="tczyx", memory_mib=1)
        for budget in (0, 1, None):
            with radelta.open(path, cache_mib=budget) as source:
                for key in (Ellipsis, (slice(None, None, -1), slice(None, None, -1),
                                      slice(None, None, -2)), (1, slice(None), 7)):
                    np.testing.assert_array_equal(source[key], image[key])

    def test_interleaved_channel_reads_with_default_and_explicit_cache_budgets(self):
        image = np.arange(3 * 4 * 7 * 9, dtype=np.uint16).reshape(3, 4, 7, 9)
        path = self.root / "channels.rdlt"
        radelta.write(path, image, axes="czyx", memory_mib=1)
        for budget in (None, 0, 1):
            with radelta.open(path, cache_mib=budget) as source:
                for z, c in itertools.product(range(4), range(3)):
                    np.testing.assert_array_equal(source[c, z], image[c, z])
            np.testing.assert_array_equal(radelta.read(path, cache_mib=budget), image)
        for budget, error in ((-1, ValueError), (1.5, TypeError), (2**128, ValueError)):
            with self.assertRaises(error):
                radelta.open(path, cache_mib=budget)

    def test_xy_slices_and_full_planes_match_numpy(self):
        image = np.arange(3 * 7 * 9, dtype=np.uint16).reshape(3, 7, 9)
        path = self.root / "slices.rdlt"
        radelta.write(path, image)
        slices = [slice(None), slice(None, None, -1), slice(None, None, -2),
                  slice(1, None, 2), slice(-2, None, -2), slice(-20, 20, 3),
                  slice(20, -20, -3), slice(5, 1, -2), slice(0, 0), slice(-1, -1, -1)]
        with radelta.open(path) as source:
            for z, y, x in itertools.product([slice(None), slice(None, None, -1), 1], slices, slices):
                np.testing.assert_array_equal(source[z, y, x], image[z, y, x])

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def test_streaming_roundtrip_metadata_and_lazy_slicing(self):
        for axes in ("yx", "zyx", "tyx", "tczyx", "xztcy"):
            shape = tuple(dict(t=2, c=2, z=3, y=5, x=7)[a] for a in axes)
            image = np.arange(np.prod(shape), dtype=np.uint16).reshape(shape).astype(">u2")
            path = self.root / (axes+".rdlt")
            radelta.write(path, image, axes=axes, metadata=b"user metadata\0\xff", memory_mib=1)
            np.testing.assert_array_equal(radelta.read(path), image)
            self.assertEqual(path.read_bytes()[:4], b"RDS3")
            with radelta.open(path) as source:
                self.assertEqual(source.shape, image.shape)
                self.assertEqual(source.axes, axes)
                self.assertEqual(source.metadata, b"user metadata\0\xff")
                for key in (Ellipsis, -1, (slice(None, None, -1), Ellipsis),
                            (Ellipsis, slice(1, None, 2)), (slice(0, 0), Ellipsis),
                            tuple(1 for _ in shape)):
                    np.testing.assert_array_equal(source[key], image[key])
                with self.assertRaises(IndexError):
                    source[image.shape[0]]
                with self.assertRaises(TypeError):
                    source[True]
            self.assertTrue(source.closed)
            with self.assertRaises(ValueError):
                source[...]
            with self.assertRaises(ValueError):
                radelta.read(path, max_output_bytes=image.nbytes-1)

    def test_region_decodes_only_selected_planes(self):
        image = np.arange(6*7*9, dtype=np.uint16).reshape(6, 7, 9)
        path = self.root / "partial.rdlt"
        radelta.write(path, image)
        with radelta.open(path) as source, patch.object(source, "_plane", wraps=source._plane) as read_plane:
            np.testing.assert_array_equal(source[2:4, 1:3, 3:5], image[2:4, 1:3, 3:5])
            self.assertEqual(read_plane.call_count, 2)

    def test_bytes_are_interoperable_with_file_reader(self):
        image = np.arange(63, dtype=np.uint16).reshape(7, 9)
        path = self.root / "memory.rdlt"
        path.write_bytes(radelta.compress(image, metadata=b"array bytes"))
        with radelta.open(path) as f:
            np.testing.assert_array_equal(f[:], image)
            self.assertEqual(f.metadata, b"array bytes")

    def test_failure_cleanup_and_no_clobber(self):
        image = np.arange(3*7*9, dtype=np.uint16).reshape(3, 7, 9)
        path = self.root / "safe.rdlt"
        radelta.write(path, image)
        original = path.read_bytes()
        with self.assertRaises(FileExistsError):
            radelta.write(path, image)
        for hook in ("radelta._file.os.replace", "radelta._file.check_file"):
            with patch(hook, side_effect=OSError("injected failure")):
                with self.assertRaises(OSError):
                    radelta.write(path, image, overwrite=True)
            self.assertEqual(path.read_bytes(), original)
        self.assertEqual(list(self.root.glob(".radelta-*")), [])
        radelta.write(path, image+1, overwrite=True)
        np.testing.assert_array_equal(radelta.read(path), image+1)

    def test_lossy_streaming_write(self):
        image = np.arange(3*7*9, dtype=np.uint16).reshape(3, 7, 9)*31
        path = self.root / "lossy.rdlt"
        radelta.write(path, image, lossy=True, offset_adu=100, gain_e_per_adu=.46)
        expected = radelta.decompress(radelta.compress(image, lossy=True, offset_adu=100, gain_e_per_adu=.46))
        np.testing.assert_array_equal(radelta.read(path), expected)


if __name__ == "__main__":
    unittest.main()
