package org.radelta.imagej;

import ij.ImagePlus;
import ij.ImageStack;
import org.junit.Test;

import java.io.IOException;
import java.math.BigInteger;
import java.nio.file.Files;
import java.nio.file.Path;

import static org.junit.Assert.*;

public class RadeltaOpeningTest {
    private Path writeImage() throws IOException {
        ImageStack stack = new ImageStack(2, 1);
        stack.addSlice("first", new short[] {1, 7});
        stack.addSlice("second", new short[] {11, 23});
        Path path = Files.createTempFile("radelta-opening-", ".rdlt");
        RadeltaExporter.saveLossless(new ImagePlus("source", stack), path.toString(), 1);
        return path;
    }

    private void assertClosed(RadeltaFile file) throws IOException {
        try {
            file.readPlane(0, 0, 0);
            fail("native reader is still open");
        } catch (IOException e) {
            assertEquals("Radelta file is closed", e.getMessage());
        }
    }

    @Test
    public void cancellationAndChooserFailureCloseNativeFile() throws Exception {
        Path path = writeImage();
        final RadeltaFile[] selected = new RadeltaFile[1];
        try {
            assertNull(RadeltaImageFactory.openForDisplay(path.toString(), file -> {
                selected[0] = file;
                assertEquals(BigInteger.valueOf(8), file.pixelBytesBig());
                return null;
            }));
            assertClosed(selected[0]);
            try {
                RadeltaImageFactory.openForDisplay(path.toString(), file -> {
                    selected[0] = file;
                    throw new IllegalStateException("chooser failed");
                });
                fail("expected chooser failure");
            } catch (IllegalStateException expected) {
                assertEquals("chooser failed", expected.getMessage());
            }
            assertClosed(selected[0]);
        } finally {
            Files.deleteIfExists(path);
        }
    }

    @Test
    public void fullLoadOwnsPixelsAndReleasesNativeFile() throws Exception {
        Path path = writeImage();
        final RadeltaFile[] selected = new RadeltaFile[1];
        try {
            ImagePlus image = RadeltaImageFactory.openForDisplay(path.toString(), file -> {
                selected[0] = file;
                return RadeltaImageFactory.OpenMode.MEMORY;
            });
            try {
                assertClosed(selected[0]);
                Files.delete(path);
                assertFalse(image.getStack().isVirtual());
                assertArrayEquals(new short[] {1, 7}, (short[]) image.getStack().getPixels(1));
                assertArrayEquals(new short[] {11, 23}, (short[]) image.getStack().getPixels(2));
                assertEquals("second", image.getStack().getSliceLabel(2));
            } finally {
                image.close();
            }
        } finally {
            Files.deleteIfExists(path);
        }
    }

    @Test
    public void headlessOpeningRemainsVirtualWithoutDialog() throws Exception {
        Path path = writeImage();
        try {
            ImagePlus image = RadeltaImageFactory.openForDisplay(path.toString());
            try {
                assertTrue(image.getStack().isVirtual());
                assertEquals("virtual", image.getProperty("Radelta.OpenMode"));
                assertArrayEquals(new short[] {11, 23}, (short[]) image.getStack().getPixels(2));
            } finally {
                ((RadeltaVirtualStack) image.getStack()).close();
                image.close();
            }
        } finally {
            Files.deleteIfExists(path);
        }
    }
}
