package org.radelta.imagej;

import static org.junit.Assert.*;

import ij.ImagePlus;
import ij.ImageStack;
import ij.measure.Calibration;

import org.junit.Test;

import java.io.*;
import java.nio.*;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.util.Arrays;

public class RadeltaMetadataTest {
    private ImagePlus image() {
        ImageStack stack = new ImageStack(3, 2);
        for (int i = 0; i < 8; i++) {
            short[] pixels = new short[6];
            for (int j = 0; j < pixels.length; j++) pixels[j] = (short) ((i * 6 + j) * (i * 6 + j));
            stack.addSlice("plane " + i, pixels);
        }
        ImagePlus image = new ImagePlus("Acquisition α", stack);
        image.setDimensions(2, 2, 2);
        Calibration c = image.getCalibration();
        c.pixelWidth = .125;
        c.pixelHeight = .25;
        c.pixelDepth = 1.5;
        c.setUnit("um");
        c.setTimeUnit("ms");
        c.frameInterval = 15;
        c.fps = 7;
        c.xOrigin = 2;
        c.yOrigin = 3;
        c.zOrigin = 4;
        c.setInvertY(true);
        c.loop = true;
        c.info = "camera calibration";
        c.setFunction(Calibration.STRAIGHT_LINE, new double[] {10, 2}, "electrons", true);
        image.setProperty("Info", "Acquisition metadata\nμ = 12");
        image.setProperty("exposure", Double.valueOf(15.5));
        image.setProperty("vendor", new byte[] {0, 1, (byte) 255});
        image.setProp("channel.name", "GFP");
        image.setProperty(RadeltaMetadata.ORIGINAL, new byte[] {0, (byte) 255, 3, 4, 0});
        return image;
    }

    private void check(ImagePlus expected, ImagePlus actual) {
        Calibration a = expected.getCalibration(), b = actual.getCalibration();
        assertEquals(a.pixelWidth, b.pixelWidth, 0);
        assertEquals(a.pixelHeight, b.pixelHeight, 0);
        assertEquals(a.pixelDepth, b.pixelDepth, 0);
        assertEquals(a.getUnit(), b.getUnit());
        assertEquals(a.frameInterval, b.frameInterval, 0);
        assertEquals(a.getTimeUnit(), b.getTimeUnit());
        assertEquals(a.xOrigin, b.xOrigin, 0);
        assertEquals(a.yOrigin, b.yOrigin, 0);
        assertEquals(a.zOrigin, b.zOrigin, 0);
        assertEquals(a.fps, b.fps, 0);
        assertEquals(a.loop, b.loop);
        assertEquals(a.getInvertY(), b.getInvertY());
        assertEquals(a.info, b.info);
        assertEquals(a.getFunction(), b.getFunction());
        assertArrayEquals(a.getCoefficients(), b.getCoefficients(), 0);
        assertEquals(a.zeroClip(), b.zeroClip());
        assertEquals(a.getValueUnit(), b.getValueUnit());
        assertEquals(expected.getTitle(), actual.getTitle());
        assertEquals(expected.getProperty("Info"), actual.getProperty("Info"));
        assertEquals(expected.getProperty("exposure"), actual.getProperty("exposure"));
        assertArrayEquals(
                (byte[]) expected.getProperty("vendor"), (byte[]) actual.getProperty("vendor"));
        assertEquals(expected.getProp("channel.name"), actual.getProp("channel.name"));
        assertArrayEquals(
                (byte[]) expected.getProperty(RadeltaMetadata.ORIGINAL),
                (byte[]) actual.getProperty(RadeltaMetadata.ORIGINAL));
        for (int i = 1; i <= expected.getStackSize(); i++) {
            assertArrayEquals(
                    (short[]) expected.getStack().getPixels(i),
                    (short[]) actual.getStack().getPixels(i));
            assertEquals(expected.getStack().getSliceLabel(i), actual.getStack().getSliceLabel(i));
        }
    }

    private void close(ImagePlus image) {
        image.close();
        if (image.getStack() instanceof RadeltaVirtualStack)
            ((RadeltaVirtualStack) image.getStack()).close();
    }

    @Test
    public void nativeLosslessLossyAndResavePreserveMetadata() throws Exception {
        ImagePlus original = image();
        Path path = Files.createTempFile("radelta-fiji-", ".rdlt");
        Path resaved = Files.createTempFile("radelta-fiji-resaved-", ".rdlt");
        try {
            for (boolean inMemory : new boolean[] {false, true}) {
                for (boolean lossy : new boolean[] {false, true}) {
                    if (lossy) RadeltaExporter.saveLossy(original, path.toString(), 0, 1, 2, 1);
                    else RadeltaExporter.saveLossless(original, path.toString(), 1);
                    ImagePlus decoded = RadeltaImageFactory.openForDisplay(path.toString(), inMemory);
                    assertEquals(!inMemory, decoded.getStack().isVirtual());
                    assertEquals(inMemory ? "memory" : "virtual", decoded.getProperty("Radelta.OpenMode"));
                    assertEquals(2, decoded.getNChannels());
                    assertEquals(2, decoded.getNSlices());
                    assertEquals(2, decoded.getNFrames());
                    try {
                        check(original, decoded);
                        // Edits override the snapshot but do not lose opaque source bytes.
                        decoded.getCalibration().pixelDepth = 2.5;
                        decoded.setProperty("Info", "edited acquisition");
                        decoded.getStack().setSliceLabel("edited label", 1);
                        RadeltaExporter.saveLossless(decoded, resaved.toString(), 1);
                        ImagePlus again = RadeltaImageFactory.openBase(resaved.toString());
                        try {
                            check(decoded, again);
                            assertArrayEquals(
                                    RadeltaMetadata.capture(decoded), RadeltaMetadata.capture(again));
                        } finally {
                            close(again);
                        }
                        ImagePlus adopted = new ImagePlus();
                        RadeltaImageFactory.adoptInto(adopted, decoded);
                        check(decoded, adopted);
                    } finally {
                        close(decoded);
                    }
                }
            }
        } finally {
            Files.deleteIfExists(path);
            Files.deleteIfExists(resaved);
        }
    }

    @Test
    public void customCalibrationAndCorruptSnapshot() throws Exception {
        ImagePlus source = image();
        float[] table = new float[65536];
        for (int i = 0; i < table.length; i++) table[i] = i * .75f;
        source.getCalibration().setCTable(table, "intensity");
        byte[] data = RadeltaMetadata.capture(source);
        ImagePlus target = image();
        RadeltaMetadata.apply(target, data);
        assertArrayEquals(table, target.getCalibration().getCTable(), 0);
        for (byte[] invalid :
                new byte[][] {Arrays.copyOf(data, 12), Arrays.copyOf(data, data.length - 1)}) {
            try {
                RadeltaMetadata.apply(target, invalid);
                fail("accepted truncated metadata");
            } catch (IOException expected) {
            }
        }
    }

    private byte[] tiffMetadata(String description) throws IOException {
        byte[] desc = (description + "\0").getBytes(StandardCharsets.UTF_8);
        ByteBuffer out =
                ByteBuffer.allocate(8 + 1 + 8 + 8 * 8 + 8 + 8 + 20 + desc.length)
                        .order(ByteOrder.LITTLE_ENDIAN);
        out.put("RDTIFF01".getBytes(StandardCharsets.US_ASCII));
        out.put((byte) 1);
        out.putLong(8);
        for (int i = 0; i < 8; i++) out.putLong(i);
        out.putLong(1);
        out.putLong(1);
        out.putShort((short) 270);
        out.putShort((short) 2);
        out.putLong(desc.length);
        out.put(desc);
        out.putLong(0);
        return out.array();
    }

    @Test
    public void tiffDescriptionsApplyCalibrationAndRemainOpaque() throws Exception {
        for (String description :
                new String[] {
                    "ImageJ=1.54\nunit=um\nspacing=3.25\nfinterval=4.5\n",
                    "<OME><Image><Pixels PhysicalSizeX=\"0.125\" PhysicalSizeZ=\"3.25\""
                        + " TimeIncrement=\"4.5\"/></Image></OME>"
                }) {
            ImagePlus image = image();
            byte[] data = tiffMetadata(description);
            RadeltaMetadata.apply(image, data);
            assertEquals(3.25, image.getCalibration().pixelDepth, 0);
            assertEquals(4.5, image.getCalibration().frameInterval, 0);
            assertEquals(description, image.getProperty("Info"));
            assertArrayEquals(data, (byte[]) image.getProperty(RadeltaMetadata.ORIGINAL));
            byte[] wrapped = RadeltaMetadata.capture(image);
            ImagePlus restored = image();
            RadeltaMetadata.apply(restored, wrapped);
            check(image, restored);
        }
    }
}
