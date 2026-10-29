import copy
import ctypes as ct
from concurrent.futures import ThreadPoolExecutor
from contextlib import redirect_stderr
import io
import json
import os
from pathlib import Path
import struct
import subprocess
import sys
import tempfile
import threading
from types import SimpleNamespace
import unittest
from unittest.mock import patch
import warnings
import zlib

import numpy as np
import tifffile
import zarr

from radelta_zarr import RadeltaCodec, RadeltaNumcodec, create_ome_image, repack, write_image
from radelta_zarr._native import decode_chunk, encode_chunk, library, validate_options
from radelta_zarr.__main__ import main
from radelta_zarr._tiff import TiffSource
from radelta_zarr.ome import _array, _write_chunks, chunk_slices


class NativeCase(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.path = Path(self.tmp.name)
        self.rng = np.random.default_rng(17)

    def tearDown(self):
        self.tmp.cleanup()


class CodecTests(NativeCase):
    def test_uint8_chunks_arrays_pyramids_and_lossy_saturation(self):
        data = np.arange(512, dtype=np.uint16).astype(np.uint8).reshape(2, 16, 16)
        for lossy in (False, True):
            opts = validate_options(dict(lossy=lossy))
            expected = np.minimum(np.floor(np.sqrt(data.astype(float))+.5)**2, 255).astype(np.uint8) if lossy else data
            encoded = encode_chunk(data, "zyx", opts)
            decoded = decode_chunk(encoded, data.shape, "zyx", "u1", opts)
            self.assertEqual(decoded.dtype, np.uint8)
            np.testing.assert_array_equal(decoded, expected)
            with self.assertRaises(ValueError):
                decode_chunk(encoded, data.shape, "zyx", "u2", opts)
            codec = RadeltaNumcodec(data.shape, dtype="u1", **opts)
            self.assertEqual(codec.decode(codec.encode(data)), expected.tobytes())
            for version in (2, 3):
                group = zarr.open_group(str(self.path / f"bytes-{lossy}-{version}"), mode="w", zarr_format=version)
                write_image(group, data, chunks=(1, 7, 9), levels=1 if lossy else 2, **opts)
                self.assertEqual(group["0"].dtype, np.uint8)
                np.testing.assert_array_equal(group["0"][:], expected)
                if not lossy:
                    self.assertEqual(group["1"].dtype, np.uint8)
                    expected_child = ((data[:,0::2,0::2].astype(np.uint32)+data[:,1::2,0::2]+data[:,0::2,1::2]+data[:,1::2,1::2]+2)//4).astype(np.uint8)
                    np.testing.assert_array_equal(group["1"][:], expected_child)

    def test_native_frame_bytes_and_encoded_buffer_inputs(self):
        pixels = self.rng.integers(0, 65536, (3, 7, 9), dtype=np.uint16)
        options = validate_options({})
        dll = library()
        pointer, size = ct.POINTER(ct.c_uint8)(), ct.c_size_t()
        status = dll.radelta_compress_u16_nd_alloc(
            pixels.ctypes.data_as(ct.POINTER(ct.c_uint16)), pixels.size,
            9, 7, 3, 1, 1, None, None, ct.byref(pointer), ct.byref(size))
        self.assertEqual(status, 0)
        try:
            payload = ct.string_at(pointer, size.value)
        finally:
            dll.radelta_buffer_free(pointer, size.value)
        prefix = struct.pack("<4sBBHQ", b"RDZ1", 1, 0, 0, pixels.nbytes)
        expected = prefix + struct.pack("<I", zlib.crc32(payload, zlib.crc32(prefix))) + payload
        encoded = encode_chunk(pixels, "zyx", options)
        self.assertIsInstance(encoded, bytes)
        self.assertEqual(encoded, expected)
        spaced = bytearray(2 * len(encoded))
        spaced[::2] = encoded
        for source in (encoded, bytearray(encoded), memoryview(encoded), memoryview(spaced)[::2]):
            np.testing.assert_array_equal(decode_chunk(source, pixels.shape, "zyx", "u2", options), pixels)

    def test_axes_contexts_byteorders_and_edges(self):
        for fmt in (2, 3):
            for axes, shape in (("yx", (13, 17)), ("zyx", (5, 13, 17)),
                                ("tyx", (2, 13, 17)), ("tzyx", (2, 3, 13, 17)),
                                ("tcyx", (2, 3, 13, 17)), ("tczyx", (2, 2, 3, 13, 17)),
                                ("tcxyz", (2, 2, 17, 13, 3))):
                for endian in ("<", ">"):
                    for context in ("mean", "mean-signs", "signed3", "signs"):
                        with self.subTest(fmt=fmt, axes=axes, endian=endian, context=context):
                            data = self.rng.integers(0, 65536, shape, dtype=np.uint16).astype(endian+"u2")
                            chunks = tuple(min(n, 7 if a == "y" else 9 if a == "x" else 2) for n, a in zip(shape, axes))
                            g = zarr.open_group(self.path/"data", mode="w", zarr_format=fmt)
                            arr = _array(g, "0", shape=shape, chunks=chunks, axes=axes, dtype=data.dtype, context=context)
                            arr[:] = data
                            fresh = zarr.open_group(self.path/"data", mode="r", use_consolidated=False)["0"]
                            np.testing.assert_array_equal(fresh[:], data)
                            roi = tuple(slice(1, n, 2) for n in shape)
                            np.testing.assert_array_equal(fresh[roi], data[roi])

    def test_numcodec_out_and_fortran_layout(self):
        x = self.rng.integers(0, 65536, (3, 7, 9), dtype=np.uint16)
        for order in ("C", "F"):
            codec = RadeltaNumcodec(x.shape, "zyx", order=order)
            encoded = codec.encode(np.array(x, order=order))
            out = np.empty_like(x, order=order)
            self.assertIs(codec.decode(encoded, out), out)
            np.testing.assert_array_equal(out, x)
            g = zarr.open_group(self.path/order, mode="w", zarr_format=2)
            arr = _array(g, "0", shape=(5, 13, 17), chunks=x.shape, axes="zyx", order=order)
            data = self.rng.integers(0, 65536, arr.shape, dtype=np.uint16)
            arr[:] = data
            np.testing.assert_array_equal(zarr.open_group(self.path/order)["0"][:], data)

    def test_entrypoints_in_a_fresh_reader(self):
        for fmt in (2, 3):
            target = self.path/str(fmt)
            g = zarr.open_group(target, mode="w", zarr_format=fmt)
            write_image(g, np.full((3, 7, 9), 1234, np.uint16))
            code = "import zarr,sys; a=zarr.open_group(sys.argv[1])['0']; assert (a[:]==1234).all()"
            result = subprocess.run([sys.executable, "-c", code, str(target)], capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)

    def test_lossy_calibration(self):
        x = np.arange(4000, dtype=np.uint16).reshape(5, 20, 40)
        expected_q = np.floor(np.sqrt(np.maximum((x.astype(float)-100)*.46, 0))+.5)
        expected = np.clip(np.floor(expected_q**2/.46+100+.5), 0, 65535).astype(np.uint16)
        for fmt in (2, 3):
            g = zarr.open_group(self.path/str(fmt), mode="w", zarr_format=fmt)
            write_image(g, x, chunks=(3, 11, 17), lossy=True, offset_adu=100, gain_e_per_adu=.46, noise_step=2)
            np.testing.assert_array_equal(zarr.open_group(self.path/str(fmt))["0"][:], expected)

    def test_all_zero_lossy_chunks_are_quantised(self):
        for fmt in (2, 3):
            g = zarr.open_group(self.path/str(fmt), mode="w", zarr_format=fmt)
            write_image(g, np.zeros((3, 11, 13), dtype=np.uint16), chunks=(2, 7, 9),
                        lossy=True, offset_adu=100, gain_e_per_adu=.46, noise_step=2)
            np.testing.assert_array_equal(g["0"][:], np.full((3, 11, 13), 100, np.uint16))

    def test_partial_updates_and_resize(self):
        for fmt in (2, 3):
            g = zarr.open_group(self.path/str(fmt), mode="w", zarr_format=fmt)
            arr = create_ome_image(g, shape=(1, 1, 3, 13, 17), axes="tczyx", chunks=(1, 1, 2, 7, 9))
            arr.resize((2, 1, 3, 13, 17))
            arr[1, 0, 1, 3:11, 4:14] = 65535
            expected = np.zeros(arr.shape, dtype=np.uint16)
            expected[1, 0, 1, 3:11, 4:14] = 65535
            np.testing.assert_array_equal(zarr.open_group(self.path/str(fmt))["0"][:], expected)

    def test_partial_read_skips_unrelated_corrupt_chunk(self):
        for fmt in (2, 3):
            target = self.path/str(fmt)
            g = zarr.open_group(target, mode="w", zarr_format=fmt)
            arr = create_ome_image(g, shape=(4, 14, 18), chunks=(2, 7, 9))
            arr[:] = np.ones(arr.shape, dtype=np.uint16)*1234
            key = target/"0"/("1.1.1" if fmt == 2 else "c/1/1/1")
            self.assertTrue(key.is_file())
            key.write_bytes(b"broken chunk")
            fresh = zarr.open_group(target)["0"]
            np.testing.assert_array_equal(fresh[:1, :2, :3], np.full((1, 2, 3), 1234, np.uint16))
            with self.assertRaises(ValueError):
                fresh[3:4, 13:14, 17:18]

    def test_sharding(self):
        g = zarr.open_group(self.path/"shards", mode="w", zarr_format=3)
        arr = _array(g, "0", shape=(5, 13, 17), chunks=(1, 7, 9), shards=(2, 14, 18), axes="zyx")
        x = self.rng.integers(0, 65536, arr.shape, dtype=np.uint16)
        arr[:] = x
        np.testing.assert_array_equal(zarr.open_group(self.path/"shards")["0"][2:4, 3:11, 4:14], x[2:4, 3:11, 4:14])

    def test_checksum_schema_and_forged_geometry(self):
        x = np.arange(3*7*9, dtype=np.uint16).reshape(3, 7, 9)
        opts = validate_options({})
        encoded = encode_chunk(x, "zyx", opts)
        for corrupt in (encoded[:12], encoded[:-3], encoded[:25]+bytes([encoded[25]^1])+encoded[26:]):
            with self.assertRaises(ValueError):
                decode_chunk(corrupt, x.shape, "zyx", "u2", opts)
        with self.assertRaises(ValueError):
            decode_chunk(encoded, (3, 9, 7), "zyx", "u2", opts)
        with self.assertRaises(ValueError):
            RadeltaCodec.from_dict({"name": "radelta.radelta", "configuration": {"version": 2}})
        self.assertEqual(RadeltaCodec.from_dict(RadeltaCodec("zyx").to_dict()).to_dict(), RadeltaCodec("zyx").to_dict())

    def test_invalid_inputs_and_large_metadata_only_image(self):
        for axes in ("c", "zzxy", "abc", "tzx"):
            with self.assertRaises(ValueError):
                RadeltaCodec(axes)
        for opts in ({"noise_step": 0}, {"gain_e_per_adu": float("nan")}, {"scale_bits": 12}, {"context": "other"}):
            with self.assertRaises(ValueError):
                RadeltaCodec("zyx", **opts)
        g = zarr.open_group(self.path/"huge", mode="w", zarr_format=3)
        arr = create_ome_image(g, shape=(20000, 20000, 20000), chunks=(4, 256, 256))
        self.assertEqual(arr.shape, (20000, 20000, 20000))
        with self.assertRaises(ValueError):
            _array(g, "bad", shape=(13, 17), chunks=(7, 9), axes="yx", dtype="f4")


class OmeTests(NativeCase):
    def test_tiff_cache_reads_once_and_stays_bounded(self):
        data = np.arange(5*13*17, dtype=np.uint16).reshape(5, 13, 17)
        path = self.path/"cached.tif"
        # Exercise decoded TIFF pages, including compressed input.
        for compression in (None, "deflate"):
            tifffile.imwrite(path, data, compression=compression, photometric="minisblack", metadata=None)
            for capacity in (0, data[0].nbytes, 2*data[0].nbytes):
                with tifffile.TiffFile(path) as tif:
                    source = TiffSource(tif, cache_bytes=capacity)
                    counts = [0]*len(source.series.pages)
                    def wrapped(index, page):
                        def read():
                            counts[index] += 1
                            return page.asarray()
                        return SimpleNamespace(asarray=read)
                    source.series = SimpleNamespace(shape=source.series.shape,
                        pages=[wrapped(i, p) for i, p in enumerate(source.series.pages)])
                    for selection in chunk_slices(data.shape, (2, 7, 9)):
                        np.testing.assert_array_equal(source[selection], data[selection])
                        self.assertLessEqual(source._cached_bytes, capacity)
                    self.assertEqual(counts, [1]*5 if capacity >= 2*data[0].nbytes else [4]*4+[1 if capacity else 4])

    def test_parallel_writes_roundtrip_and_same_chunk_bytes(self):
        data = self.rng.integers(0, 4096, (5, 13, 17), dtype=np.uint16)
        for fmt in (2, 3):
            for lossy in (False, True):
                outputs = []
                for workers in (1, 4):
                    path = self.path/f"{fmt}-{lossy}-{workers}"
                    group = zarr.open_group(path, mode="w", zarr_format=fmt)
                    calls = []
                    write_image(group, data, chunks=(2, 7, 9), workers=workers, lossy=lossy,
                                offset_adu=100, gain_e_per_adu=.46,
                                progress=lambda p, s: calls.append((p, s, threading.get_ident())))
                    self.assertEqual(len(calls), 12)
                    self.assertEqual({c[2] for c in calls}, {threading.get_ident()})
                    outputs.append(path)
                a, b = (zarr.open_group(p)["0"][:] for p in outputs)
                np.testing.assert_array_equal(a, b)
                if not lossy:
                    np.testing.assert_array_equal(a, data)
                chunks = lambda p: {str(f.relative_to(p)): f.read_bytes() for f in (p/"0").rglob("*")
                                    if f.is_file() and f.name not in {"zarr.json", ".zarray", ".zattrs"}}
                self.assertEqual(chunks(outputs[0]), chunks(outputs[1]))

    def test_parallel_pipeline_is_bounded_and_drains_errors(self):
        # A barrier alone does not guarantee which writer finishes first.
        # Hold both writes so read-ahead can be measured, then fail one while
        # the other stays blocked until the pipeline has observed the error.
        source_thread = None
        active, peak, reads = 0, 0, 0
        lock = threading.Lock()
        writers_ready = threading.Barrier(3)
        fail_write = threading.Event()
        finish_write = threading.Event()
        unwinding = threading.Event()
        returned = threading.Event()
        failure = OSError("write failed")
        errors, shutdown_errors, active_after_shutdown = [], [], []

        class ObservedExecutor(ThreadPoolExecutor):
            def __exit__(self, exc_type, exc, traceback):
                shutdown_errors.append(exc)
                unwinding.set()
                result = super().__exit__(exc_type, exc, traceback)
                with lock:
                    active_after_shutdown.append(active)
                return result

        class Target:
            def __setitem__(self, selection, block):
                nonlocal active, peak
                with lock:
                    active += 1
                    peak = max(peak, active)
                try:
                    if selection < 2:
                        writers_ready.wait(timeout=30)
                        gate = fail_write if selection == 0 else finish_write
                        if not gate.wait(timeout=30):
                            raise RuntimeError("Timed out coordinating test writes")
                    if selection == 0:
                        raise failure
                finally:
                    with lock:
                        active -= 1
        def blocks():
            nonlocal reads
            for i in range(100):
                self.assertEqual(threading.get_ident(), source_thread)
                with lock:
                    reads += 1
                yield i, np.zeros((7, 9), dtype=np.uint16)

        def run_pipeline():
            nonlocal source_thread
            source_thread = threading.get_ident()
            try:
                _write_chunks(Target(), blocks(), workers=2, path="0", progress=None)
            except Exception as error:
                errors.append(error)
            finally:
                returned.set()

        with patch("radelta_zarr.ome.ThreadPoolExecutor", ObservedExecutor):
            caller = threading.Thread(target=run_pipeline, name="test-zarr-producer", daemon=True)
            caller.start()
            ready = False
            try:
                writers_ready.wait(timeout=30)
                ready = True
                with lock:
                    self.assertEqual((active, peak, reads), (2, 2, 2))
                fail_write.set()
                self.assertTrue(unwinding.wait(timeout=30), "Pipeline did not observe the failed write")
                self.assertEqual(shutdown_errors, [failure])
                self.assertFalse(returned.is_set(), "Pipeline returned before its active write finished")
                with lock:
                    self.assertEqual((active, reads), (1, 2))
            finally:
                # Release every gate even if an assertion fails; leave no
                # blocked executor threads behind on any CI platform.
                fail_write.set()
                finish_write.set()
                if not ready:
                    writers_ready.abort()
                caller.join(timeout=30)
            self.assertFalse(caller.is_alive(), "Pipeline did not finish draining its writes")
        self.assertEqual(errors, [failure])
        self.assertTrue(returned.is_set())
        self.assertEqual(active_after_shutdown, [0])
        self.assertEqual(active, 0)
        self.assertEqual(peak, 2)
        self.assertEqual(reads, 2)

    def test_pyramid_and_coordinate_centres(self):
        x = np.arange(5*13*17, dtype=np.uint16).reshape(5, 13, 17)
        for fmt in (2, 3):
            g = zarr.open_group(self.path/str(fmt), mode="w", zarr_format=fmt)
            write_image(g, x, levels=3, scale=(2,.2,.3), translation=(1,10,20),
                        units={a:"micrometer" for a in "zyx"})
            expected = x
            for i in range(3):
                np.testing.assert_array_equal(g[str(i)][:], expected)
                if i<2:
                    padded = np.pad(expected, ((0,0),(0,expected.shape[1]%2),(0,expected.shape[2]%2)), mode="edge")
                    expected = ((padded[:,0::2,0::2].astype(np.uint32)+padded[:,1::2,0::2]+padded[:,0::2,1::2]+padded[:,1::2,1::2]+2)//4).astype(np.uint16)
            ome = g.attrs.get("ome", dict(g.attrs))
            datasets = ome["multiscales"][0]["datasets"]
            np.testing.assert_allclose(datasets[2]["coordinateTransformations"][0]["scale"], (2,.8,1.2))
            np.testing.assert_allclose(datasets[2]["coordinateTransformations"][1]["translation"], (1,10.3,20.45))

    def test_repack_preserves_hierarchy_metadata_and_labels(self):
        for fmt in (2,3):
            source, target, standard = (self.path/f"{tag}{fmt}" for tag in ("source","target","standard"))
            g = zarr.open_group(source, mode="w", zarr_format=fmt)
            img = g.create_group("A/1/0")
            x = np.arange(3*13*17,dtype=np.uint16).reshape(3,13,17)
            write_image(img,x,levels=2,scale=(2,.2,.2),units={a:"micrometer" for a in "zyx"})
            labels = img.create_group("labels/nuclei")
            write_image(labels,x%7,levels=1)
            if fmt==3:
                attrs=labels.attrs["ome"];attrs["image-label"]={"source":{"image":"../../"}};labels.attrs["ome"]=attrs
            else:
                labels.attrs["image-label"]={"source":{"image":"../../"}}
            g.attrs["custom"]={"experiment":"E12.5", "nested":[1,2,3]}
            g.create_array("auxiliary",data=np.arange(20,dtype=np.uint32))
            (source/"OME").mkdir();(source/"OME"/"METADATA.ome.xml").write_text("<OME/>")
            before=copy.deepcopy(dict(img.attrs))
            with warnings.catch_warnings():
                warnings.simplefilter("ignore")
                repack(source,target,lossy=True,offset_adu=0,gain_e_per_adu=.46,noise_step=2)
            out=zarr.open_group(target)
            self.assertEqual(dict(out["A/1/0"].attrs),before)
            np.testing.assert_array_equal(out["A/1/0/labels/nuclei/0"][:],x%7)
            np.testing.assert_array_equal(out["auxiliary"][:],np.arange(20,dtype=np.uint32))
            self.assertEqual(out.attrs["custom"],g.attrs["custom"])
            self.assertEqual((target/"OME/METADATA.ome.xml").read_text(),"<OME/>")
            with warnings.catch_warnings():
                warnings.simplefilter("ignore")
                repack(target,standard,compression="zstd")
            np.testing.assert_array_equal(zarr.open_group(standard)["A/1/0/0"][:],out["A/1/0/0"][:])

    def cli(self,*args,success=True):
        result=subprocess.run([sys.executable,"-m","radelta_zarr",*map(str,args)],capture_output=True,text=True)
        self.assertEqual(result.returncode==0,success,result.stderr)
        return result

    def test_tiff_cli_reorder_calibration_export(self):
        data=np.arange(2*3*4*13*17,dtype=np.uint16).reshape(2,3,4,13,17)
        source=self.path/"source.ome.tif"
        tifffile.imwrite(source,data,ome=True,photometric="minisblack",metadata={"axes":"TZCYX", "PhysicalSizeX":.2,"PhysicalSizeY":.3,"PhysicalSizeZ":2.,"PhysicalSizeXUnit":"µm","PhysicalSizeYUnit":"µm","PhysicalSizeZUnit":"µm","Channel":{"Name":["one","two","three","four"]}})
        for fmt in (2,3):
            target, restored=self.path/f"test{fmt}.ome.zarr",self.path/f"restored{fmt}.ome.tif"
            self.cli("encode",source,target,"--zarr-format",fmt,"--chunks","1,1,2,7,9")
            g=zarr.open_group(target)
            np.testing.assert_array_equal(g["0"][:],data.transpose(0,2,1,3,4))
            ome=g.attrs.get("ome",dict(g.attrs))
            np.testing.assert_allclose(ome["multiscales"][0]["datasets"][0]["coordinateTransformations"][0]["scale"],(1,1,2,.3,.2))
            self.cli("decode",target,restored)
            np.testing.assert_array_equal(tifffile.imread(restored),data.transpose(0,2,1,3,4))
            with tifffile.TiffFile(restored) as tif:
                self.assertIn('PhysicalSizeX="0.2"',tif.ome_metadata)
                self.assertIn('PhysicalSizeXUnit="µm"',tif.ome_metadata)
            self.assertIn("radelta.radelta",self.cli("info",target).stdout)

    def test_cli_failure_output_protection(self):
        source=self.path/"input.tif"
        tifffile.imwrite(source,np.arange(5*13*17,dtype=np.uint16).reshape(5,13,17),photometric="minisblack",metadata=None)
        target=self.path/"out.zarr"
        self.cli("encode",source,target,"--levels","2")
        metadata=(target/"zarr.json").read_bytes()
        self.cli("encode",source,target,success=False)
        self.assertEqual((target/"zarr.json").read_bytes(),metadata)
        broken=self.path/"broken.zarr"
        self.cli("encode",source,broken,"--lossy",success=False)
        self.assertFalse(broken.exists())
        self.cli("encode",source,broken,"--chunks","1,2",success=False)
        self.assertFalse(broken.exists())
        for args in (("--workers", "0"), ("--tiff-cache-mib", "-1"), ("--tiff-cache-mib", "nan")):
            self.cli("encode", source, broken, *args, success=False)
            self.assertFalse(broken.exists())
        with patch("radelta_zarr.codecs.encode_chunk", side_effect=RuntimeError("injected codec failure")):
            with redirect_stderr(io.StringIO()) as stderr, self.assertRaises(SystemExit) as error:
                main(["encode", str(source), str(broken), "--chunks", "2,7,9", "--workers", "4"])
            self.assertEqual(error.exception.code, 1)
            self.assertIn("injected codec failure", stderr.getvalue())
            self.assertFalse(broken.exists())
        self.assertEqual(list(self.path.glob(".radelta-*")),[])
        with self.assertRaises(ValueError):
            repack(target,target/"nested")


if __name__=="__main__":
    unittest.main()
