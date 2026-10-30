#!/usr/bin/env python3
"""Test self-contained Python wheels in a clean venv outside the checkout."""
import argparse
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import venv

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kind", choices=("array", "hdf5", "combined"), required=True)
    parser.add_argument("--wheel-dir", type=Path, required=True)
    parser.add_argument("--array-wheel-dir", type=Path)
    parser.add_argument("--h5py-version", help="Optional compatibility test, e.g. 3.10.0")
    args = parser.parse_args()
    prefix = "radelta-" if args.kind == "array" else "radelta_hdf5-"
    wheels = list(args.wheel_dir.resolve().glob(prefix+"*.whl"))
    if len(wheels) != 1:
        parser.error("Expected exactly one matching wheel")
    if args.kind == "combined":
        if args.array_wheel_dir is None:
            parser.error("--combined requires --array-wheel-dir")
        arrays = list(args.array_wheel_dir.resolve().glob("radelta-*.whl"))
        if len(arrays) != 1:
            parser.error("Expected exactly one array wheel")
        wheels += arrays
    environment = dict(os.environ)
    for key in ("PYTHONPATH", "PYTHONHOME", "RADELTA_NATIVE_LIBRARY", "RADELTA_HDF5_PLUGIN_PATH",
                "HDF5_PLUGIN_PATH", "LD_LIBRARY_PATH", "DYLD_LIBRARY_PATH"):
        environment.pop(key, None)
    with tempfile.TemporaryDirectory(prefix="radelta python wheel test-") as temporary:
        work = Path(temporary)
        venv.EnvBuilder(with_pip=True).create(work / "venv")
        python = work / "venv" / ("Scripts/python.exe" if os.name == "nt" else "bin/python")
        dependencies = []
        if args.h5py_version:
            # The pinned h5py wheel is tested with NumPy's 1.x ABI.
            dependencies += ["h5py=="+args.h5py_version, "numpy<2"]
        # Verify installation against bundled upstream runtimes. Missing h5py
        # or NumPy wheels must fail rather than build against a local SDK.
        subprocess.run([str(python), "-m", "pip", "install", "--only-binary=h5py,numpy",
                        *map(str, wheels), *dependencies],
                       cwd=work, env=environment, check=True)
        tests = work / "tests"
        tests.mkdir()
        if args.kind in ("array", "combined"):
            shutil.copy2(ROOT / "python/tests/test_api.py", tests)
            smoke = """from pathlib import Path
import radelta
from radelta._native import library
assert Path(library()._name).resolve().is_relative_to(Path(radelta.__file__).resolve().parent/'natives')
"""
            subprocess.run([str(python), "-c", smoke], cwd=work, env=environment, check=True)
        if args.kind in ("hdf5", "combined"):
            for name in ("test_bdv.py", "test_embedded.py"):
                shutil.copy2(ROOT / "hdf5/tests" / name, tests)
            smoke = """import gc
import os
from importlib.metadata import version
# Registration must work without HDF5's automatic plugin loader, whose
# Windows dependency search differs from ctypes and delvewheel.
if tuple(map(int, version('h5py').split('.')[:2])) >= (3, 9):
    os.environ['HDF5_PLUGIN_PRELOAD'] = '::'
import h5py
import numpy as np
import radelta_hdf5
assert h5py.h5z.filter_avail(65024), 'Bundled filter was not registered'
image = np.arange(63, dtype=np.uint16).reshape(7, 9)
with h5py.File('registration-smoke.h5', 'w') as f:
    data = radelta_hdf5.create_dataset(f, 'pixels', data=image)
    data.id.close()
    gc.collect()
    radelta_hdf5.configure_plugin()  # Repeated setup must preserve live users.
    np.testing.assert_array_equal(f['pixels'][:], image)
"""
            subprocess.run([str(python), "-c", smoke], cwd=work, env=environment, check=True)
        if args.kind == "combined":
            shutil.copy2(ROOT / "python/tests/test_hdf5_api.py", tests)
        subprocess.run([str(python), "-m", "unittest", "discover", "-s", "tests", "-v"],
                       cwd=work, env=environment, check=True)
    print(f"Installed {args.kind} wheel tests passed without native-library or plugin-path overrides", flush=True)


if __name__ == "__main__":
    main()
