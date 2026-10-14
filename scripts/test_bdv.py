#!/usr/bin/env python3
"""Export fixtures and read them with Fiji's pinned BDV XML/HDF5 loader."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

import h5py
import numpy as np

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "hdf5/python"))
sys.path.insert(0, str(ROOT / "hdf5/tests"))

from radelta_hdf5 import configure_plugin, export_bdv
from test_bdv import reference_mean
from test_hdf5_fiji import FIJI_SITE, pinned_jars


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--plugin-path", type=Path, required=True,
                        help="SDK filter built against this Python's HDF5")
    parser.add_argument("--fiji-plugin-path", type=Path, required=True,
                        help="embedded filter directory (no second HDF5 runtime)")
    parser.add_argument("--jar-cache", type=Path, default=ROOT / "_deps/bdv-test-jars")
    args = parser.parse_args()
    configure_plugin(args.plugin_path)
    resources = json.loads((ROOT / "hdf5/tests/bdv-reader-jars.json").read_text())
    jars = pinned_jars(args.jar_cache.resolve(), resources, FIJI_SITE)
    with tempfile.TemporaryDirectory(prefix="radelta bdv test-") as temporary:
        work = Path(temporary)
        classes = work / "classes"
        classes.mkdir()
        cp = os.pathsep.join(map(str, jars))
        subprocess.run(["javac", "--release", "8", "-cp", cp, "-d", str(classes),
                        str(ROOT / "hdf5/tests/BdvReadTest.java")], check=True)
        data = (np.arange(2*2*5*17*19, dtype=np.uint32)*37 % 65536).reshape(2, 2, 5, 17, 19).astype(np.uint16)
        source = work / "input.h5"
        with h5py.File(source, "w") as f:
            f.create_dataset("images", data=data)
        for lossy in (False, True):
            folder = work / ("lossy" if lossy else "lossless")
            folder.mkdir()
            xml, _ = export_bdv(source, folder / "sample.xml", chunks=(3, 7, 9),
                                resolutions=((1, 1, 1), (1, 2, 2), (2, 4, 4)),
                                voxel_size=(2, .5, .25), lossy=lossy,
                                offset_adu=100, gain_e_per_adu=.46, noise_step=2)
            lines = []
            for t, c in np.ndindex((2, 2)):
                image = data[t, c]
                if lossy:
                    q = np.floor(np.sqrt(np.maximum((image.astype(float)-100)*.46, 0))+.5)
                    image = np.clip(np.floor(q*q/.46+100+.5), 0, 65535).astype(np.uint16)
                for level, ratio in enumerate(((1, 1, 1), (1, 2, 2), (2, 2, 2))):
                    if level:
                        image = reference_mean(image, ratio)
                    name = f"t{t}-c{c}-l{level}.raw"
                    image.astype("<u2").tofile(folder / name)
                    lines.append("\t".join(map(str, (t, c, level, *image.shape[::-1], name))))
            (folder / "expected.tsv").write_text("\n".join(lines)+"\n")
            env = dict(os.environ)
            for key in ("HDF5_PLUGIN_PATH", "LD_LIBRARY_PATH", "DYLD_LIBRARY_PATH"):
                env.pop(key, None)
            subprocess.run(["java", "-Djava.awt.headless=true", "-cp", str(classes)+os.pathsep+cp,
                            "BdvReadTest", str(xml), str(folder), str(args.fiji_plugin_path.resolve())],
                           env=env, check=True, timeout=120)
            print(f"{folder.name}: BDV integration passed", flush=True)


if __name__ == "__main__":
    main()
