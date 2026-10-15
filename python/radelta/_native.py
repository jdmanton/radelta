"""Private ctypes binding to the existing Radelta C ABI."""
import ctypes as ct
from functools import lru_cache
import os
from pathlib import Path

U8 = ct.POINTER(ct.c_uint8)
U16 = ct.POINTER(ct.c_uint16)
SIZE = ct.POINTER(ct.c_size_t)
DIMS = [ct.POINTER(ct.c_uint32)] * 5
HANDLE = ct.c_void_p


class Options(ct.Structure):
    _fields_ = [("block_depth", ct.c_uint32), ("context_mode", ct.c_uint32), ("scale_bits", ct.c_uint32)]


class LossyOptions(ct.Structure):
    _fields_ = Options._fields_ + [("offset_adu", ct.c_double), ("gain_e_per_adu", ct.c_double),
                                  ("noise_step", ct.c_double)]


class FileInfo(ct.Structure):
    _fields_ = [(name, ct.c_uint32) for name in ("x", "y", "z", "c", "t", "format", "flags")]


@lru_cache(maxsize=1)
def library():
    package = Path(__file__).resolve().parent
    explicit = os.environ.get("RADELTA_NATIVE_LIBRARY")
    names = ("radelta.dll", "libradelta.so", "libradelta.dylib",
             "radelta_native.dll", "libradelta_native.so", "libradelta_native.dylib")
    directories = (package / "natives", package.parents[1] / "native/target/release")
    candidates = [Path(explicit)] if explicit else [p/n for p in directories for n in names]
    selected = next((p.resolve() for p in candidates if p.is_file()), None)
    if selected is None:
        raise RuntimeError("Radelta native library not found. Install the self-contained wheel, "
                           "or set RADELTA_NATIVE_LIBRARY for a source checkout.")
    dll = ct.CDLL(str(selected))
    signatures = {
        "radelta_compress_u16_nd_alloc": ([U16, ct.c_size_t, *([ct.c_uint32]*5),
                                           ct.POINTER(Options), ct.POINTER(LossyOptions),
                                           ct.POINTER(U8), SIZE], ct.c_int32),
        "radelta_buffer_free": ([U8, ct.c_size_t], None),
        "radelta_decompress_u16_nd": ([U8, ct.c_size_t, U16, ct.c_size_t, SIZE, *DIMS], ct.c_int32),
        "radelta_get_metadata": ([U8, ct.c_size_t, U8, ct.c_size_t, SIZE], ct.c_int32),
        "radelta_set_metadata": ([U8, ct.c_size_t, U8, ct.c_size_t, U8, ct.c_size_t, SIZE], ct.c_int32),
        "radelta_file_open": ([ct.c_char_p, ct.POINTER(HANDLE)], ct.c_int32),
        "radelta_file_close": ([HANDLE], None),
        "radelta_file_get_info": ([HANDLE, ct.POINTER(FileInfo)], ct.c_int32),
        "radelta_file_read_plane_u16": ([HANDLE, *([ct.c_uint32]*3), U16, ct.c_uint64], ct.c_int32),
        "radelta_file_get_metadata": ([HANDLE, U8, ct.c_size_t, SIZE], ct.c_int32),
        "radelta_file_last_error": ([ct.c_void_p, ct.c_uint64], ct.c_uint64),
        "radelta_writer_create_lossless_u16": ([ct.c_char_p, *([ct.c_uint32]*6), ct.POINTER(HANDLE)], ct.c_int32),
        "radelta_writer_create_lossy_u16": ([ct.c_char_p, *([ct.c_uint32]*5), *([ct.c_double]*3),
                                            ct.c_uint32, ct.POINTER(HANDLE)], ct.c_int32),
        "radelta_writer_write_plane_u16": ([HANDLE, *([ct.c_uint32]*3), U16, ct.c_uint64], ct.c_int32),
        "radelta_writer_set_metadata": ([HANDLE, U8, ct.c_size_t], ct.c_int32),
        "radelta_writer_finish": ([HANDLE], ct.c_int32),
        "radelta_writer_close": ([HANDLE], None),
    }
    for name, (arguments, result) in signatures.items():
        try:
            fn = getattr(dll, name)
        except AttributeError as exc:
            raise RuntimeError("Native library does not match this Radelta Python release; "
                               "install its bundled wheel or update RADELTA_NATIVE_LIBRARY") from exc
        fn.argtypes, fn.restype = arguments, result
    return dll


def bytes_buffer(data):
    if not isinstance(data, (bytes, bytearray, memoryview)):
        raise TypeError("Expected bytes, bytearray or memoryview")
    data = bytes(data)
    return (ct.c_uint8 * len(data)).from_buffer_copy(data)


def metadata_bytes(function, *args):
    size = ct.c_size_t()
    status = function(*args, None, 0, ct.byref(size))
    if status not in (0, 1):
        raise ValueError("Cannot read Radelta metadata")
    if not size.value:
        return b""
    output = (ct.c_uint8 * size.value)()
    if function(*args, output, size.value, ct.byref(size)) != 0:
        raise ValueError("Cannot read Radelta metadata")
    return bytes(output)


def check_file(status):
    if status != 0:
        dll = library()
        size = dll.radelta_file_last_error(None, 0)
        message = ct.create_string_buffer(max(1, size))
        dll.radelta_file_last_error(message, len(message))
        raise OSError(message.value.decode("utf-8", errors="replace") or f"Radelta error {status}")


def pixel_pointer(array):
    return array.ctypes.data_as(U16)
