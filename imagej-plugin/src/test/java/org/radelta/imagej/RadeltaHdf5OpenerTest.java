package org.radelta.imagej;

import java.io.File;
import org.junit.Test;
import org.scijava.io.location.BytesLocation;
import org.scijava.io.location.FileLocation;
import static org.junit.Assert.*;

/** Opening rules that need no HDF5 runtime; Fiji integration tests cover the rest. */
public class RadeltaHdf5OpenerTest {
    @Test
    public void layoutsComeFromTheAxesAttributeOrTheRank() {
        assertEquals("zyx", RadeltaHdf5Opener.validLayout("ZYX", 3));
        assertEquals("tczyx", RadeltaHdf5Opener.validLayout(" TCZYX ", 5));
        assertEquals("zcyx", RadeltaHdf5Opener.validLayout("zcyx", 4));
        assertNull(RadeltaHdf5Opener.validLayout("zyx", 4));
        assertNull(RadeltaHdf5Opener.validLayout("zzyx", 4));
        assertNull(RadeltaHdf5Opener.validLayout("tczy", 4));
        assertNull(RadeltaHdf5Opener.validLayout("abyx", 4));
        assertNull(RadeltaHdf5Opener.validLayout(null, 3));
        assertEquals("yx", RadeltaHdf5Opener.defaultLayout(2));
        assertEquals("czyx", RadeltaHdf5Opener.defaultLayout(4));
        assertNull(RadeltaHdf5Opener.defaultLayout(1));
        assertNull(RadeltaHdf5Opener.defaultLayout(6));
    }

    @Test
    public void onlyHdf5NamesAreConsidered() {
        assertTrue(RadeltaHdf5Opener.hasHdf5Extension("/data/image.h5"));
        assertTrue(RadeltaHdf5Opener.hasHdf5Extension("C:\\data\\IMAGE.HDF5"));
        assertTrue(RadeltaHdf5Opener.hasHdf5Extension("image.he5"));
        assertFalse(RadeltaHdf5Opener.hasHdf5Extension("image.h5j"));
        assertFalse(RadeltaHdf5Opener.hasHdf5Extension("image.rdlt"));
        assertFalse(RadeltaHdf5Opener.hasHdf5Extension(null));
    }

    @Test
    public void detectionFailsSafelyWithoutHdf5() throws Exception {
        // Maven tests have no HDF5 reader: the hook must decline, not throw,
        // so that other readers still receive the file.
        File file = File.createTempFile("radelta-opener-", ".h5");
        try {
            assertFalse(RadeltaHdf5Opener.supports(file.getPath()));
            assertFalse(new RadeltaHdf5IOPlugin().supportsOpen(file.getPath()));
            assertFalse(new RadeltaHdf5IOPlugin().supportsOpen(new FileLocation(file)));
        } finally {
            assertTrue(file.delete());
        }
    }

    @Test
    public void hooksAcceptResolvedFileLocations() {
        // Fiji's "SciJava I/O" option resolves paths to locations first.
        RadeltaIOPlugin rdlt = new RadeltaIOPlugin();
        assertTrue(rdlt.supportsOpen(new FileLocation(new File("image.rdlt"))));
        assertFalse(rdlt.supportsOpen(new FileLocation(new File("image.tif"))));
        assertFalse(rdlt.supportsOpen(new BytesLocation(new byte[0], "image.rdlt")));
        assertFalse(new RadeltaHdf5IOPlugin().supportsOpen(new BytesLocation(new byte[0], "image.h5")));
    }
}
