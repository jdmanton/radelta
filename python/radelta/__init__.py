"""Lossless and calibrated-lossy compression for uint16 NumPy arrays.

compress/decompress operate on bytes; read/write operate on files. open returns
a lazy, read-only dataset and should be used as a context manager. HDF5 support
is optional and loaded only for HDF5 paths. NumPy is the only core dependency.
"""
from ._array import compress, decompress
from ._file import Dataset, open, read, write

__version__ = "0.7.7"
__all__ = ["compress", "decompress", "read", "write", "open", "Dataset", "__version__"]
