"""Pinned Java dependencies shared by release packaging and integration tests."""
import hashlib
import urllib.request

FIJI_SITE = "https://sites.imagej.net/Fiji/"
JARS = (
    ("jars/jhdf5-19.04.1.jar-20220912165414", "0c20636d3388ebaa3c941f6a2c867fc0420ebbc2a5768c14f008f87069e6cecc"),
    ("jars/base-18.09.0.jar-20220912165414", "c73d01ffb427e5b7008003b4eaf9303c1febd883100bf81752ba71f41c701148"),
    ("jars/commons-lang3-3.18.0.jar-20260216133515", "4eeeae8d20c078abb64b015ec158add383ac581571cddc45c68f0c9ae0230720"),
    ("jars/commons-io-2.20.0.jar-20260716221137", "df90bba0fe3cb586b7f164e78fe8f8f4da3f2dd5c27fa645f888100ccc25dd72"),
    ("jars/ij-1.54p.jar-20250219110715", "2e1a09961dfb41cee66ddc821b2577a41a072566ce45a49bae69267099741e20"),
    ("plugins/HDF5_Vibez-1.1.1.jar-20220912165414", "f1072d69843847a8bc3646c54885c67ec28f1224347f09af58bc5f2c6a67b545"),
)
SCIJAVA_JARS = (
    ("org/scijava/scijava-common/2.100.1/scijava-common-2.100.1.jar", "af53bb31087dd46bfe15645c8b7cc69fe837795516816c60f5be78b9ff0ce6a3"),
    ("org/scijava/parsington/3.1.0/parsington-3.1.0.jar", "7943c91a1e62918ea47d0a08f6b5989a8b388e019216441002236528f769c72a"),
)


def download(cache, filename, url, sha256):
    """Return a cached download after verifying its SHA256, including cache hits."""
    cache.mkdir(parents=True, exist_ok=True)
    path = cache / filename
    if not path.is_file():
        with urllib.request.urlopen(url, timeout=120) as response:
            data = response.read()
        if hashlib.sha256(data).hexdigest() != sha256:
            raise RuntimeError(f"SHA256 mismatch for downloaded {filename}")
        path.write_bytes(data)
    if hashlib.sha256(path.read_bytes()).hexdigest() != sha256:
        raise RuntimeError(f"SHA256 mismatch for cached {filename}")
    return path


def pinned_jars(cache, resources, base_url):
    return [download(cache, resource.rsplit("/", 1)[1].split(".jar", 1)[0] + ".jar",
                     base_url + resource, digest) for resource, digest in resources]


def fiji_jars(cache):
    return pinned_jars(cache, JARS, FIJI_SITE)
