"""TIFF integration must remain discoverable without optional image packages."""
import builtins
import importlib.util
import os
from pathlib import Path
import sys
from types import ModuleType
import unittest
from unittest.mock import patch

TEST_MODULE = Path(__file__).with_name("test_uint8_tiff.py")


class TiffDiscoveryTests(unittest.TestCase):
    def test_missing_optional_dependencies_skip_without_import_errors(self):
        real_import = builtins.__import__
        for missing in ("numpy", "tifffile", "imagecodecs"):
            with self.subTest(missing=missing):
                def import_without_dependency(name, *args, **kwargs):
                    if name == missing:
                        raise ModuleNotFoundError(f"No module named '{name}'", name=name)
                    return real_import(name, *args, **kwargs)

                spec = importlib.util.spec_from_file_location("optional_uint8_tiff", TEST_MODULE)
                module = importlib.util.module_from_spec(spec)
                dependencies = {name: ModuleType(name) for name in ("numpy", "tifffile", "imagecodecs")}
                # An existing path makes the dependency guard, rather than the
                # missing-executable guard, responsible for skipping the test.
                with patch.dict(sys.modules, dependencies), \
                        patch.dict(os.environ, {"RADELTA_CLI": str(TEST_MODULE)}), \
                        patch("builtins.__import__", side_effect=import_without_dependency):
                    spec.loader.exec_module(module)
                result = unittest.TestResult()
                unittest.defaultTestLoader.loadTestsFromModule(module).run(result)
                self.assertGreater(result.testsRun, 0)
                self.assertEqual(result.errors, [])
                self.assertEqual(result.failures, [])
                self.assertEqual(len(result.skipped), result.testsRun)
                self.assertTrue(all(f"missing {missing}" in reason for _, reason in result.skipped))


if __name__ == "__main__":
    unittest.main()
