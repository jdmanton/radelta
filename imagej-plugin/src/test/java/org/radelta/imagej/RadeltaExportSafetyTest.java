package org.radelta.imagej;

import ij.ImagePlus;
import ij.ImageStack;
import org.junit.Test;
import java.io.IOException;
import java.io.UncheckedIOException;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Arrays;
import java.util.Comparator;
import java.util.stream.Stream;
import static org.junit.Assert.*;

public class RadeltaExportSafetyTest {
    private Path writeImage(Path root) throws IOException {
        ImageStack stack = new ImageStack(300, 300);
        for (int z = 0; z < 3; z++) {
            short[] pixels = new short[90000];
            Arrays.fill(pixels, (short)(1000 + z));
            stack.addSlice("plane " + z, pixels);
        }
        Path path = root.resolve("source.rdlt");
        RadeltaExporter.saveLossless(new ImagePlus("source", stack), path.toString(), 1);
        return path;
    }

    private void save(ImagePlus image, Path path, boolean lossy) throws IOException {
        if (lossy) RadeltaExporter.saveLossy(image, path.toString(), 0, 1, 2, 1);
        else RadeltaExporter.saveLossless(image, path.toString(), 1);
    }

    private void clean(Path root) throws IOException {
        try (Stream<Path> paths = Files.walk(root)) {
            for (Path p : (Iterable<Path>)paths.sorted(Comparator.reverseOrder())::iterator) Files.delete(p);
        }
    }

    private void noTemporaryFiles(Path root) throws IOException {
        try (Stream<Path> paths = Files.list(root)) {
            assertFalse(paths.anyMatch(p -> p.getFileName().toString().startsWith(".radelta-")));
        }
    }

    @Test
    public void virtualSourceAndHardLinksCannotBeOverwritten() throws Exception {
        Path root = Files.createTempDirectory("radelta-source-protection-");
        ImagePlus virtual = null;
        try {
            Path source = writeImage(root);
            byte[] original = Files.readAllBytes(source);
            virtual = RadeltaImageFactory.openBase(source.toString());
            Path link = root.resolve("source-link.rdlt");
            Files.createLink(link, source);
            for (boolean lossy : new boolean[] {false, true}) {
                for (Path target : new Path[] {source, link}) {
                    try { save(virtual, target, lossy); fail("source alias was overwritten"); }
                    catch (IOException expected) { assertTrue(expected.getMessage().contains("source file")); }
                    assertArrayEquals(original, Files.readAllBytes(source));
                    assertArrayEquals(original, Files.readAllBytes(link));
                }
            }
            assertEquals(1002, ((short[])virtual.getStack().getPixels(3))[0]);
            noTemporaryFiles(root);
        } finally {
            if (virtual != null) ((RadeltaVirtualStack)virtual.getStack()).close();
            clean(root);
        }
    }

    @Test
    public void hdf5SavesCannotOverwriteVirtualSources() throws Exception {
        // The source check runs before any HDF5 work, so no HDF5 runtime is needed.
        Path root = Files.createTempDirectory("radelta-hdf5-source-protection-");
        ImagePlus virtual = null;
        try {
            Path source = writeImage(root);
            byte[] original = Files.readAllBytes(source);
            virtual = RadeltaImageFactory.openBase(source.toString());
            Path link = root.resolve("source-link.h5");
            Files.createLink(link, source);
            for (Path target : new Path[] {source, link}) {
                try { RadeltaHdf5.save(virtual, target.toString(), "/images"); fail("source alias was overwritten"); }
                catch (IOException expected) { assertTrue(expected.getMessage().contains("source file")); }
                try { RadeltaHdf5.saveLossy(virtual, target.toString(), "/images", 0, 1, 2); fail("source alias was overwritten"); }
                catch (IOException expected) { assertTrue(expected.getMessage().contains("source file")); }
                assertArrayEquals(original, Files.readAllBytes(target));
            }
            assertEquals(1002, ((short[])virtual.getStack().getPixels(3))[0]);
            noTemporaryFiles(root);
        } finally {
            if (virtual != null) ((RadeltaVirtualStack)virtual.getStack()).close();
            clean(root);
        }
    }

    @Test
    public void corruptPlaneThrowsAndFailedExportsPreserveDestinations() throws Exception {
        Path root = Files.createTempDirectory("radelta-corrupt-export-");
        ImagePlus virtual = null;
        try {
            Path source = writeImage(root);
            byte[] bytes = Files.readAllBytes(source);
            ByteBuffer header = ByteBuffer.wrap(bytes).order(ByteOrder.LITTLE_ENDIAN);
            int second = 64 + (int)header.getLong(56);
            int secondEnd = second + 24 + (int)header.getLong(second + 16);
            bytes[secondEnd - 1] ^= 1;
            Files.write(source, bytes);
            virtual = RadeltaImageFactory.openBase(source.toString());
            try { virtual.getStack().getProcessor(2); fail("corrupt chunk returned pixels"); }
            catch (UncheckedIOException expected) { assertTrue(expected.getCause().getMessage().contains("checksum mismatch")); }
            byte[] previous = "previous complete image".getBytes("UTF-8");
            for (boolean lossy : new boolean[] {false, true}) {
                for (boolean existing : new boolean[] {false, true}) {
                    Path target = root.resolve("result.rdlt");
                    if (existing) Files.write(target, previous);
                    try { save(virtual, target, lossy); fail("corrupt source was exported"); }
                    catch (IOException expected) { assertTrue(expected.getCause().getMessage().contains("checksum mismatch")); }
                    if (existing) { assertArrayEquals(previous, Files.readAllBytes(target)); Files.delete(target); }
                    else assertFalse(Files.exists(target));
                    noTemporaryFiles(root);
                }
            }
            assertArrayEquals(bytes, Files.readAllBytes(source));
        } finally {
            if (virtual != null) ((RadeltaVirtualStack)virtual.getStack()).close();
            clean(root);
        }
    }

    @Test
    public void corruptFirstPlaneFailsOpeningAsAnIOException() throws Exception {
        Path root = Files.createTempDirectory("radelta-corrupt-first-");
        try {
            Path source = writeImage(root);
            byte[] bytes = Files.readAllBytes(source);
            int end = 64 + (int)ByteBuffer.wrap(bytes).order(ByteOrder.LITTLE_ENDIAN).getLong(56);
            bytes[end - 1] ^= 1;
            Files.write(source, bytes);
            try { RadeltaImageFactory.openBase(source.toString()); fail("corrupt first chunk opened"); }
            catch (IOException expected) { assertTrue(expected.getMessage().contains("checksum mismatch")); }
        } finally { clean(root); }
    }
}
