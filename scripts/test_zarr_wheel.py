#!/usr/bin/env python3
"""Install a repaired wheel in a fresh venv and test outside the checkout."""
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
    parser.add_argument("--wheel-dir", type=Path, default=ROOT/"dist/zarr-wheels")
    args = parser.parse_args()
    wheels = list(args.wheel_dir.resolve().glob("radelta_zarr-*.whl"))
    if len(wheels) != 1:
        parser.error(f"Expected one repaired wheel, found {len(wheels)}")
    environment = dict(os.environ)
    for name in ("RADELTA_NATIVE_LIBRARY", "PYTHONPATH", "PYTHONHOME"):
        environment.pop(name, None)
    with tempfile.TemporaryDirectory(prefix="radelta-wheel-test-") as temporary:
        root = Path(temporary)
        env = root/"venv"
        venv.EnvBuilder(with_pip=True).create(env)
        python = env/("Scripts/python.exe" if os.name == "nt" else "bin/python")
        subprocess.run([str(python), "-m", "pip", "install", str(wheels[0])],
                       cwd=root, env=environment, check=True)
        # Loading must come from this wheel, not an environment override,
        # development build or a shared library in the working directory.
        smoke = """from pathlib import Path
import numpy as np
import zarr
import radelta_zarr
from radelta_zarr._native import library
package = Path(radelta_zarr.__file__).resolve().parent
assert Path(library()._name).resolve().is_relative_to(package / 'natives')
data = np.arange(3*13*17, dtype=np.uint16).reshape(3,13,17)
for fmt in (2,3):
    group = zarr.open_group(f'smoke-{fmt}', mode='w', zarr_format=fmt)
    radelta_zarr.write_image(group, data, chunks=(2,7,9))
    np.testing.assert_array_equal(group['0'][:], data)
print('Installed wheel loaded its bundled library; Zarr v2/v3 round trips passed.')
"""
        subprocess.run([str(python), "-c", smoke], cwd=root, env=environment, check=True)
        shutil.copytree(ROOT/"zarr/tests", root/"tests", ignore=shutil.ignore_patterns("__pycache__"))
        subprocess.run([str(python), "-m", "unittest", "discover", "-s", "tests", "-v"],
                       cwd=root, env=environment, check=True)


if __name__ == "__main__":
    main()
