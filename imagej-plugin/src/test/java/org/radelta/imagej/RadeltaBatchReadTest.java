package org.radelta.imagej;

import ij.ImagePlus;
import ij.ImageStack;
import org.junit.Test;
import java.nio.file.Files;
import java.nio.file.Path;
import java.io.IOException;
import static org.junit.Assert.*;

public class RadeltaBatchReadTest {
    private short[] pixels(int page) {
        return new short[] {(short) (page * 100), (short) (page * 100 + 1)};
    }

    private Path image() throws Exception {
        ImageStack stack = new ImageStack(2, 1);
        for (int page = 0; page < 3 * 17 * 3; ++page) stack.addSlice("page " + page, pixels(page));
        ImagePlus source = new ImagePlus("batch", stack);
        source.setDimensions(3, 17, 3);
        Path path = Files.createTempFile("radelta-batch-", ".rdlt");
        RadeltaExporter.saveLossless(source, path.toString(), 1);
        return path;
    }

    @Test
    public void memoryLoadCrossesBatchBoundaryInImageJOrder() throws Exception {
        Path path = image();
        try {
            ImagePlus loaded = RadeltaImageFactory.openForDisplay(path.toString(), true);
            try {
                assertEquals(3, loaded.getNChannels());
                assertEquals(17, loaded.getNSlices());
                assertEquals(3, loaded.getNFrames());
                for (int i = 0; i < 153; ++i) {
                    assertArrayEquals(pixels(i), (short[]) loaded.getStack().getPixels(i + 1));
                    assertEquals("page " + i, loaded.getStack().getSliceLabel(i + 1));
                }
            } finally { loaded.close(); }
        } finally { Files.deleteIfExists(path); }
    }

    @Test
    public void arbitraryAndRepeatedCoordinatesOwnTheirPixelsAndCloseCleanly() throws Exception {
        Path path = image();
        RadeltaFile file = RadeltaFile.open(path.toString());
        try {
            short[][] first = file.readPlanes(new int[] {2, 2, 16, 0, 0, 0, 2, 2, 16});
            assertArrayEquals(pixels(152), first[0]);
            assertArrayEquals(pixels(0), first[1]);
            assertArrayEquals(first[0], first[2]);
            assertNotSame(first[0], first[2]);
            first[0][0] = 1;
            assertArrayEquals(pixels(152), first[2]);
            assertArrayEquals(pixels(152), file.readPlanes(new int[] {2, 2, 16, 2, 2, 16})[0]);
            assertEquals(0, file.readPlanes(new int[0]).length);
            try {
                file.readPlanes(new int[] {0, 0, 0, 0, 0, 17});
                fail("invalid coordinate accepted");
            } catch (IOException expected) { /* native validation */ }
            assertArrayEquals(pixels(0), file.readPlane(0, 0, 0));
        } finally { file.close(); Files.deleteIfExists(path); }
        try {
            file.readPlanes(new int[] {0, 0, 0});
            fail("closed file accepted");
        } catch (IOException expected) { assertEquals("Radelta file is closed", expected.getMessage()); }
    }
}
