import importlib.util
from pathlib import Path
import tempfile
import unittest
from zipfile import ZipFile

ROOT = Path(__file__).resolve().parents[2]


def module(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


installer = module("install_fiji", ROOT / "imagej-plugin/install_fiji.py")
stager = module("stage_imagej_natives", ROOT / "scripts/stage_imagej_natives.py")


def jar(path, members):
    path.parent.mkdir(parents=True, exist_ok=True)
    with ZipFile(path, "w") as z:
        for name, contents in members.items():
            z.writestr(name, contents)


class BundleInstallerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.work = Path(self.temp.name)
        self.bundle, self.fiji = self.work / "bundle", self.work / "Fiji.app"
        (self.fiji / "jars").mkdir(parents=True)
        (self.fiji / "plugins").mkdir()
        for path, signature in installer.DEPENDENCIES:
            jar(self.bundle / path, {signature: b"H5PLprepend hdf5read"})
        jar(self.bundle / "plugins/radelta-imagej-0.7.0.jar", {"plugins.config": b"new"})

    def test_reuses_host_libraries_and_disables_old_plugin(self):
        host_io = self.fiji / "jars/commons-io-newer.jar"
        jar(host_io, {"org/apache/commons/io/IOUtils.class": b"existing"})
        old = self.fiji / "plugins/radelta-imagej-0.6.0.jar"
        jar(old, {"plugins.config": b"old"})
        original = host_io.read_bytes()
        installer.install(self.bundle, self.fiji)
        self.assertEqual(host_io.read_bytes(), original)
        self.assertFalse((self.fiji / "jars/commons-io-2.20.0.jar").exists())
        self.assertTrue(old.with_suffix(".jar.disabled").is_file())
        self.assertFalse(old.exists())
        # Installing the same bundle twice remains free of duplicate JARs.
        installer.install(self.bundle, self.fiji)
        self.assertEqual(len(list((self.fiji / "jars").glob("jhdf5-*.jar"))), 1)

    def test_old_reader_is_rejected_before_mutation(self):
        old = self.fiji / "jars/jhdf5-19.04.0.jar"
        jar(old, {"hdf/hdf5lib/H5.class": b"H5PLprepend"})
        with self.assertRaisesRegex(RuntimeError, "Outdated"):
            installer.install(self.bundle, self.fiji)
        self.assertEqual(list((self.fiji / "plugins").iterdir()), [])
        self.assertEqual(list((self.fiji / "jars").iterdir()), [old])

    def test_duplicate_native_reader_is_rejected(self):
        for name in ("jhdf5-19.04.1.jar", "other-reader.jar"):
            jar(self.fiji / "jars" / name, {"hdf/hdf5lib/H5.class": b"H5PLprepend"})
        with self.assertRaisesRegex(RuntimeError, "Duplicate"):
            installer.install(self.bundle, self.fiji)
        self.assertEqual(list((self.fiji / "plugins").iterdir()), [])


class NativeStagingTests(unittest.TestCase):
    def test_selects_exact_native_and_decoder_members_for_all_platforms(self):
        with tempfile.TemporaryDirectory() as temp:
            work = Path(temp)
            native, hdf5, resources = work / "native", work / "hdf5", work / "resources"
            for platform, arch, library, decoder in stager.TARGETS:
                name = f"radelta-0.7.0-{platform}-{arch}"
                reader = f"radelta-hdf5-fiji-0.7.0-{platform}-{arch}"
                jar(native / (name + ".zip"), {f"{name}/{library}": b"core",
                    f"{name}/python/radelta_zarr/_lib/{library}": b"wrong nested copy"})
                jar(hdf5 / (reader + ".zip"), {f"{reader}/plugins/{decoder}": b"decoder",
                    f"{reader}/HDF5_LICENSE.txt": b"licence"})
            stager.stage(native, hdf5, resources, "0.7.0")
            for platform, arch, library, decoder in stager.TARGETS:
                self.assertEqual((resources / "natives" / f"{platform}-{arch}" / library).read_bytes(), b"core")
                self.assertEqual((resources / "hdf5" / f"{platform}-{arch}" / decoder).read_bytes(), b"decoder")

    def test_missing_archive_does_not_partially_stage_resources(self):
        with tempfile.TemporaryDirectory() as temp:
            work = Path(temp)
            for platform, arch, library, decoder in stager.TARGETS[:-1]:
                name = f"radelta-0.7.0-{platform}-{arch}"
                reader = f"radelta-hdf5-fiji-0.7.0-{platform}-{arch}"
                jar(work / "native" / (name + ".zip"), {f"{name}/{library}": b"core"})
                jar(work / "hdf5" / (reader + ".zip"), {f"{reader}/plugins/{decoder}": b"decoder",
                    f"{reader}/HDF5_LICENSE.txt": b"licence"})
            with self.assertRaises(FileNotFoundError):
                stager.stage(work / "native", work / "hdf5", work / "resources", "0.7.0")
            self.assertFalse((work / "resources").exists())


if __name__ == "__main__":
    unittest.main()
