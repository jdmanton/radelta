package org.radelta.imagej;

import ij.ImagePlus;
import org.junit.Test;

import java.io.IOException;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.util.Arrays;

import static org.junit.Assert.*;

/** Actual TIFF -> CLI -> ImageJ tests, including reordered planes and nested SubIFDs. */
public class RadeltaPyramidTest {
    private static void tag(ByteBuffer b, int id, int type, int count, int value) {
        b.putShort((short) id).putShort((short) type).putInt(count).putInt(value);
    }

    private static void ifd(ByteBuffer b, int offset, int x, int y, int value, int pixels,
                            int next, int[] children, int descriptionLength) {
        b.position(offset);
        b.putShort((short) (9 + (children.length > 0 ? 1 : 0)
                + (descriptionLength > 0 ? 1 : 0) + (offset >= 1100 ? 1 : 0)));
        if (offset >= 1100) tag(b, 254, 4, 1, 1);
        tag(b, 256, 4, 1, x);
        tag(b, 257, 4, 1, y);
        tag(b, 258, 3, 1, 16);
        tag(b, 259, 3, 1, 1);
        tag(b, 262, 3, 1, 1);
        if (descriptionLength > 0) tag(b, 270, 2, descriptionLength, 5000);
        tag(b, 273, 4, 1, pixels);
        tag(b, 277, 3, 1, 1);
        tag(b, 278, 4, 1, y);
        tag(b, 279, 4, 1, x * y * 2);
        if (children.length > 0) {
            tag(b, 330, 4, children.length, children.length == 1 ? children[0] : offset + 220);
            if (children.length > 1) {
                int end = b.position();
                b.position(offset + 220);
                for (int child : children) b.putInt(child);
                b.position(end);
            }
        }
        b.putInt(next);
        b.position(pixels);
        for (int i = 0; i < x * y; i++) b.putShort((short) (value + i));
    }

    private static byte[] fixture(boolean incomplete) {
        ByteBuffer b = ByteBuffer.allocate(8192).order(ByteOrder.LITTLE_ENDIAN);
        b.putShort((short) 0x4949).putShort((short) 42).putInt(8);
        String xml = "<OME><Image><Pixels DimensionOrder=\"XYCZT\" Type=\"uint16\""
                + " SizeX=\"8\" SizeY=\"6\" SizeC=\"2\" SizeZ=\"2\" SizeT=\"1\""
                + " PhysicalSizeX=\"0.25\" PhysicalSizeY=\"0.5\" PhysicalSizeZ=\"2\">"
                + "<Channel ID=\"Channel:0\" SamplesPerPixel=\"1\"/>"
                + "<Channel ID=\"Channel:1\" SamplesPerPixel=\"1\"/>"
                + "<TiffData IFD=\"0\" PlaneCount=\"4\"/></Pixels></Image></OME>\0";
        byte[] description = xml.getBytes(StandardCharsets.UTF_8);
        b.position(5000);
        b.put(description);
        for (int p = 0; p < 4; p++) {
            int sub = 1100 + p * 768, pixels = 6000 + p * 256;
            ifd(b, sub, 4, 3, 1000 + 100 * p, pixels + 100, 0, new int[0], 0);
            ifd(b, sub + 256, 2, 2, 2000 + 100 * p, pixels + 140, 0,
                    new int[] {sub + 512}, 0);
            ifd(b, sub + 512, 1, 1, 3000 + 100 * p, pixels + 160, 0, new int[0], 0);
            ifd(b, 8 + p * 256, 8, 6, 100 * p, pixels, p < 3 ? 8 + (p + 1) * 256 : 0,
                    incomplete && p == 3 ? new int[] {sub} : new int[] {sub, sub + 256},
                    p == 0 ? description.length : 0);
        }
        return b.array();
    }

    private static void cli(String... args) throws Exception {
        String library = System.getenv("RADELTA_NATIVE_LIBRARY");
        assertNotNull("Native/CLI test build must be supplied", library);
        String exe = System.getProperty("os.name").startsWith("Windows") ? "radelta.exe" : "radelta";
        // The matrix deliberately reuses its private temporary output paths.
        String[] command = new String[args.length + 2];
        command[0] = Paths.get(library).toAbsolutePath().getParent().resolve(exe).toString();
        command[1] = "--force";
        System.arraycopy(args, 0, command, 2, args.length);
        Path log = Files.createTempFile("radelta-pyramid-cli-", ".log");
        try {
            Process process = new ProcessBuilder(command).redirectErrorStream(true)
                    .redirectOutput(log.toFile()).start();
            int status = process.waitFor();
            assertEquals(new String(Files.readAllBytes(log), StandardCharsets.UTF_8), 0, status);
        } finally {
            Files.delete(log);
        }
    }

    private static void close(ImagePlus image) {
        if (image.getStack() instanceof RadeltaVirtualStack)
            ((RadeltaVirtualStack) image.getStack()).close();
        image.close();
    }

    @Test
    public void opensStoredLevelsInBothModesWithCorrectPixelsOrderingAndCalibration() throws Exception {
        Path dir = Files.createTempDirectory("radelta-pyramid-imagej-");
        Path input = dir.resolve("input.ome.tif"), encoded = dir.resolve("image.rdlt");
        Path saved = dir.resolve("resaved.rdlt"), restored = dir.resolve("restored.tif");
        try {
            Files.write(input, fixture(false));
            for (boolean stream : new boolean[] {false, true}) {
                if (stream) cli("encode", input.toString(), encoded.toString(), "--stream");
                else cli("encode", input.toString(), encoded.toString());
                for (boolean memory : new boolean[] {false, true}) {
                    for (int level = 0; level < 4; level++) {
                        ImagePlus image = RadeltaImageFactory.openForDisplay(encoded.toString(), memory, level);
                        try {
                            int[] widths = {8, 4, 2, 1}, heights = {6, 3, 2, 1};
                            assertEquals(widths[level], image.getWidth());
                            assertEquals(heights[level], image.getHeight());
                            assertEquals(2, image.getNChannels());
                            assertEquals(2, image.getNSlices());
                            assertEquals(1, image.getNFrames());
                            assertEquals(!memory, image.getStack().isVirtual());
                            assertEquals("12", image.getProperty("Radelta.StoredPyramidImages"));
                            assertEquals("4", image.getProperty("Radelta.ResolutionCount"));
                            assertEquals(.25 * 8 / widths[level], image.getCalibration().pixelWidth, 1e-12);
                            assertEquals(.5 * 6 / heights[level], image.getCalibration().pixelHeight, 1e-12);
                            assertEquals(2, image.getCalibration().pixelDepth, 0);
                            for (int p = 0; p < 4; p++) {
                                short[] actual = (short[]) image.getStack().getPixels(p + 1);
                                for (int i = 0; i < actual.length; i++)
                                    assertEquals(1000 * level + 100 * p + i, Short.toUnsignedInt(actual[i]));
                            }
                            if (memory && !stream) {
                                RadeltaExporter.saveLossless(image, saved.toString(), 1);
                                ImagePlus again = RadeltaImageFactory.openForDisplay(saved.toString(), true);
                                try {
                                    assertEquals(level == 0 ? "4" : "1", again.getProperty("Radelta.ResolutionCount"));
                                    assertEquals(image.getCalibration().pixelWidth, again.getCalibration().pixelWidth, 0);
                                    assertArrayEquals((short[]) image.getStack().getPixels(4),
                                            (short[]) again.getStack().getPixels(4));
                                } finally { close(again); }
                                cli("decode", saved.toString(), restored.toString());
                            }
                        } finally { close(image); }
                    }
                }
                // Bad selection is an IOException and releases its reader.
                try {
                    RadeltaImageFactory.openForDisplay(encoded.toString(), false, 4);
                    fail("accepted missing level");
                } catch (IOException expected) {
                    assertTrue(expected.getMessage().contains("resolution"));
                }
            }
        } finally {
            for (Path p : Arrays.asList(input, encoded, saved, restored)) Files.deleteIfExists(p);
            Files.delete(dir);
        }
    }

    @Test
    public void incompleteLevelsStayPreservedButAreNotOfferedAsCompleteHyperstacks() throws Exception {
        Path dir = Files.createTempDirectory("radelta-pyramid-partial-");
        Path input = dir.resolve("input.tif"), encoded = dir.resolve("image.rdlt");
        try {
            Files.write(input, fixture(true));
            cli("encode", input.toString(), encoded.toString());
            ImagePlus image = RadeltaImageFactory.openForDisplay(encoded.toString(), true, 1);
            try {
                assertEquals("10", image.getProperty("Radelta.StoredPyramidImages"));
                assertEquals("2", image.getProperty("Radelta.ResolutionCount"));
            } finally { close(image); }
            try (RadeltaFile file = RadeltaFile.open(encoded.toString())) {
                byte[] metadata = file.readMetadata();
                for (byte[] bad : new byte[][] {Arrays.copyOf(metadata, 12), Arrays.copyOf(metadata, metadata.length - 1)}) {
                    try {
                        RadeltaPyramids.parse(bad, 4);
                        fail("accepted truncated pyramid metadata");
                    } catch (IOException expected) { }
                }
            }
        } finally {
            Files.deleteIfExists(input);
            Files.deleteIfExists(encoded);
            Files.delete(dir);
        }
    }
}
