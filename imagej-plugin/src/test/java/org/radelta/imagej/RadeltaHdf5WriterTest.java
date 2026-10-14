package org.radelta.imagej;

import ij.measure.Calibration;
import java.io.IOException;
import org.junit.Test;
import static org.junit.Assert.*;

/** Layout and parameter rules for HDF5 writing; no HDF5 runtime is needed. */
public class RadeltaHdf5WriterTest {
    @Test
    public void startsAtTheFirstNonSingletonAxis() {
        assertArrayEquals(new long[] {13, 17}, RadeltaHdf5Writer.shape(17, 13, 1, 1, 1));
        assertArrayEquals(new long[] {5, 13, 17}, RadeltaHdf5Writer.shape(17, 13, 5, 1, 1));
        assertArrayEquals(new long[] {2, 1, 13, 17}, RadeltaHdf5Writer.shape(17, 13, 1, 2, 1));
        assertArrayEquals(new long[] {3, 1, 1, 13, 17}, RadeltaHdf5Writer.shape(17, 13, 1, 1, 3));
        assertEquals("YX", RadeltaHdf5Writer.axes(2));
        assertEquals("ZYX", RadeltaHdf5Writer.axes(3));
        assertEquals("CZYX", RadeltaHdf5Writer.axes(4));
        assertEquals("TCZYX", RadeltaHdf5Writer.axes(5));
    }

    @Test
    public void defaultChunksMatchThePythonTools() {
        assertArrayEquals(new long[] {13, 17}, RadeltaHdf5Writer.defaultChunks(new long[] {13, 17}));
        assertArrayEquals(new long[] {4, 1024, 1024}, RadeltaHdf5Writer.defaultChunks(new long[] {186, 2048, 2048}));
        assertArrayEquals(new long[] {1, 1, 3, 600, 700}, RadeltaHdf5Writer.defaultChunks(new long[] {2, 3, 3, 600, 700}));
        // X and Y are capped at 1024 pixels independently of each other.
        assertArrayEquals(new long[] {4, 1, 1024}, RadeltaHdf5Writer.defaultChunks(new long[] {10, 1, 1 << 30}));
        long[] wide = RadeltaHdf5Writer.defaultChunks(new long[] {64, 1024, 1024});
        assertTrue(wide[0] * wide[1] * wide[2] * 2 <= 8L << 20);
    }

    @Test
    public void requestedChunksAreClampedToTheShape() throws IOException {
        RadeltaHdf5Writer.Settings settings = RadeltaHdf5Writer.Settings.lossless();
        settings.chunkZ = 9;
        settings.chunkY = 7;
        settings.chunkX = 0;
        assertArrayEquals(new long[] {1, 1, 5, 7, 17},
            RadeltaHdf5Writer.chunks(new long[] {2, 2, 5, 13, 17}, settings));
        assertArrayEquals(new long[] {7, 17}, RadeltaHdf5Writer.chunks(new long[] {13, 17}, settings));
    }

    @Test
    public void filterParametersDescribeTheChunkAndCalibration() {
        RadeltaHdf5Writer.Settings lossy = RadeltaHdf5Writer.Settings.lossy(100.0, 0.46, 2.0);
        int[] p = RadeltaHdf5Writer.filterParameters(lossy, new long[] {2, 1, 3, 7, 9});
        assertEquals(17, p.length);
        assertArrayEquals(new int[] {1, 1, 4, 2, 10}, java.util.Arrays.copyOf(p, 5));
        long noise = (p[9] & 0xffffffffL) | ((long) p[10] << 32);
        assertEquals(2.0, Double.longBitsToDouble(noise), 0.0);
        long gain = (p[7] & 0xffffffffL) | ((long) p[8] << 32);
        assertEquals(0.46, Double.longBitsToDouble(gain), 0.0);
        // Chunk extents X,Y,Z,C,T, then little-endian.
        assertArrayEquals(new int[] {9, 7, 3, 1, 2, 0}, java.util.Arrays.copyOfRange(p, 11, 17));
        int[] plane = RadeltaHdf5Writer.filterParameters(RadeltaHdf5Writer.Settings.lossless(), new long[] {13, 17});
        assertEquals(0, plane[1]);
        assertArrayEquals(new int[] {17, 13, 1, 1, 1, 0}, java.util.Arrays.copyOfRange(plane, 11, 17));
    }

    @Test
    public void spacingIsStoredInMicrometres() {
        Calibration calibration = new Calibration();
        assertNull(RadeltaHdf5Writer.elementSizeUm(calibration));
        calibration.setUnit("nm");
        calibration.pixelWidth = 100;
        calibration.pixelHeight = 120;
        calibration.pixelDepth = 400;
        assertArrayEquals(new float[] {0.4f, 0.12f, 0.1f}, RadeltaHdf5Writer.elementSizeUm(calibration), 1e-7f);
        calibration.setUnit("µm");
        assertArrayEquals(new float[] {400f, 120f, 100f}, RadeltaHdf5Writer.elementSizeUm(calibration), 0f);
        calibration.setUnit("inch");
        assertNull(RadeltaHdf5Writer.elementSizeUm(calibration));
        assertEquals(1e3, RadeltaHdf5Writer.micrometresPer("mm"), 0.0);
        assertTrue(Double.isNaN(RadeltaHdf5Writer.micrometresPer("pixel")));
    }

    @Test
    public void datasetNamesAreAbsoluteAndPlain() throws IOException {
        assertEquals("/images", RadeltaHdf5Writer.normalizeDataset(null));
        assertEquals("/images", RadeltaHdf5Writer.normalizeDataset("  "));
        assertEquals("/data", RadeltaHdf5Writer.normalizeDataset("data"));
        assertEquals("/a/b", RadeltaHdf5Writer.normalizeDataset("/a/b"));
        for (String bad : new String[] {"/a/", "/a//b", "/a/../b", "/a/./b"}) {
            try {
                RadeltaHdf5Writer.normalizeDataset(bad);
                fail("Accepted " + bad);
            } catch (IOException expected) {
                // Rejected as intended.
            }
        }
    }

    @Test
    public void readsTheCodecVersionFromTheFilterName() {
        assertEquals("1.2.3", RadeltaHdf5Writer.filterVersion("Radelta 1.2.3 (uint16 TCZYX, embedded)"));
        assertEquals("unknown", RadeltaHdf5Writer.filterVersion("deflate"));
        assertTrue("Radelta 1.2.3 (uint16 TCZYX, embedded)".endsWith(RadeltaHdf5Writer.EMBEDDED_SUFFIX));
        assertFalse("Radelta 1.2.3 (uint16 TCZYX)".endsWith(RadeltaHdf5Writer.EMBEDDED_SUFFIX));
    }

    @Test
    public void rejectsInvalidLossyParameters() {
        for (double[] values : new double[][] {{0, 0, 2}, {0, 1, 0}, {Double.NaN, 1, 2}, {0, Double.POSITIVE_INFINITY, 2}}) {
            try {
                RadeltaHdf5Writer.Settings.lossy(values[0], values[1], values[2]).validate();
                fail("Accepted " + java.util.Arrays.toString(values));
            } catch (IOException expected) {
                // Rejected as intended.
            }
        }
    }
}
