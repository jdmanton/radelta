package org.radelta.imagej;

import com.sun.jna.Pointer;
import com.sun.jna.ptr.PointerByReference;
import ij.IJ;
import ij.ImagePlus;
import ij.ImageStack;
import ij.io.SaveDialog;
import ij.process.ImageProcessor;
import ij.process.ShortProcessor;
import ij.process.ByteProcessor;

import java.io.IOException;

final class RadeltaExporter {
    private RadeltaExporter() {}

    static void saveLosslessInteractive(ImagePlus imp) throws IOException {
        if (imp == null) throw new IOException("No image is open.");
        SaveDialog sd = new SaveDialog("Save as Radelta (lossless)...", suggestedName(imp), ".rdlt");
        String dir = sd.getDirectory();
        String name = sd.getFileName();
        if (name == null) return;
        String path = dir + name;

        int memoryMib = 2048;
        ij.gui.GenericDialog gd = new ij.gui.GenericDialog("Save as Radelta (lossless)");
        gd.addStringField("Output", path, 50);
        gd.addNumericField("Memory budget (MiB)", memoryMib, 0);
        gd.showDialog();
        if (gd.wasCanceled()) return;

        saveLossless(imp, gd.getNextString(), (int) Math.round(gd.getNextNumber()));
    }

    static void saveLossyInteractive(ImagePlus imp) throws IOException {
        if (imp == null) throw new IOException("No image is open.");
        SaveDialog sd = new SaveDialog("Save as Radelta (lossy)...", suggestedName(imp), ".rdlt");
        String dir = sd.getDirectory();
        String name = sd.getFileName();
        if (name == null) return;
        String path = dir + name;

        ij.gui.GenericDialog gd = new ij.gui.GenericDialog("Save as Radelta (lossy)");
        gd.addStringField("Output", path, 50);
        gd.addNumericField("Offset (ADU)", 0.0, 4);
        gd.addNumericField("Gain", 1.0, 6);
        gd.addChoice("Gain units", new String[] {"e-/ADU", "ADU/e-"}, "e-/ADU");
        gd.addNumericField("Noise step", 2.0, 4);
        gd.addNumericField("Memory budget (MiB)", 2048, 0);
        gd.showDialog();
        if (gd.wasCanceled()) return;

        String out = gd.getNextString();
        double offset = gd.getNextNumber();
        double gain = gd.getNextNumber();
        String units = gd.getNextChoice();
        double noiseStep = gd.getNextNumber();
        int memoryMib = (int) Math.round(gd.getNextNumber());
        if ("ADU/e-".equals(units)) {
            if (gain == 0.0) throw new IOException("Gain in ADU/e- must be non-zero.");
            gain = 1.0 / gain;
        }
        saveLossy(imp, out, offset, gain, noiseStep, memoryMib);
    }

    static void saveLossless(ImagePlus imp, String path, int memoryMib) throws IOException {
        validateSupportedImage(imp);
        byte[] metadata = RadeltaMetadata.capture(imp);
        int nx = imp.getWidth();
        int ny = imp.getHeight();
        int nz = Math.max(1, imp.getNSlices());
        int nc = Math.max(1, imp.getNChannels());
        int nt = Math.max(1, imp.getNFrames());
        Pointer handle = null;
        PointerByReference ref = new PointerByReference();
        int rc = RadeltaNative.api().radelta_writer_create_lossless_u16(path, nx, ny, nz, nc, nt, memoryMib, ref);
        check(rc, "Could not create Radelta writer");
        handle = ref.getValue();
        try {
            check(RadeltaNative.api().radelta_writer_set_metadata(handle, metadata, new RadeltaNative.SizeT(metadata.length)), "Could not save metadata");
            exportPlanes(imp, handle, nc, nz, nt);
            rc = RadeltaNative.api().radelta_writer_finish(handle);
            check(rc, "Could not finalize Radelta file");
            IJ.showStatus("Wrote lossless Radelta: " + path);
        } finally {
            if (handle != null) {
                RadeltaNative.api().radelta_writer_close(handle);
            }
            IJ.showProgress(1.0);
        }
    }

    static void saveLossy(ImagePlus imp, String path, double offsetAdu, double gainEPerAdu,
                          double noiseStep, int memoryMib) throws IOException {
        validateSupportedImage(imp);
        if (!(Double.isFinite(offsetAdu) && Double.isFinite(gainEPerAdu) && gainEPerAdu > 0.0 &&
                Double.isFinite(noiseStep) && noiseStep > 0.0)) {
            throw new IOException("Lossy parameters must be finite; gain and noise step must be > 0.");
        }
        byte[] metadata = RadeltaMetadata.capture(imp);
        int nx = imp.getWidth();
        int ny = imp.getHeight();
        int nz = Math.max(1, imp.getNSlices());
        int nc = Math.max(1, imp.getNChannels());
        int nt = Math.max(1, imp.getNFrames());
        Pointer handle = null;
        PointerByReference ref = new PointerByReference();
        int rc = RadeltaNative.api().radelta_writer_create_lossy_u16(
                path, nx, ny, nz, nc, nt,
                offsetAdu, gainEPerAdu, noiseStep, memoryMib,
                ref);
        check(rc, "Could not create lossy Radelta writer");
        handle = ref.getValue();
        try {
            check(RadeltaNative.api().radelta_writer_set_metadata(handle, metadata, new RadeltaNative.SizeT(metadata.length)), "Could not save metadata");
            exportPlanes(imp, handle, nc, nz, nt);
            rc = RadeltaNative.api().radelta_writer_finish(handle);
            check(rc, "Could not finalize Radelta file");
            IJ.showStatus("Wrote lossy Radelta: " + path);
        } finally {
            if (handle != null) {
                RadeltaNative.api().radelta_writer_close(handle);
            }
            IJ.showProgress(1.0);
        }
    }

    private static void exportPlanes(ImagePlus imp, Pointer writerHandle, int nc, int nz, int nt) throws IOException {
        ImageStack stack = imp.getStack();
        long totalPlanes = (long) nc * (long) nz * (long) nt;
        long done = 0;
        for (int t = 1; t <= nt; ++t) {
            for (int c = 1; c <= nc; ++c) {
                for (int z = 1; z <= nz; ++z) {
                    int index = imp.getStackIndex(c, z, t);
                    short[] pixels = extractPlaneU16(stack, index);
                    int rc = RadeltaNative.api().radelta_writer_write_plane_u16(writerHandle, t - 1, c - 1, z - 1,
                            pixels, pixels.length);
                    check(rc, "Failed while writing plane C=" + c + ", Z=" + z + ", T=" + t);
                    done++;
                    IJ.showProgress(totalPlanes == 0 ? 1.0 : (double) done / (double) totalPlanes);
                    IJ.showStatus("Radelta export: " + done + "/" + totalPlanes + " planes");
                }
            }
        }
    }

    private static short[] extractPlaneU16(ImageStack stack, int oneBasedIndex) throws IOException {
        Object pixels = stack.getPixels(oneBasedIndex);
        if (pixels instanceof short[]) {
            return (short[]) pixels;
        }
        if (pixels instanceof byte[]) {
            byte[] src = (byte[]) pixels;
            short[] dst = new short[src.length];
            for (int i = 0; i < src.length; ++i) dst[i] = (short) (src[i] & 0xFF);
            return dst;
        }
        // Fallback through processor conversion for unusual virtual-stack implementations.
        ImageProcessor ip = stack.getProcessor(oneBasedIndex);
        if (ip instanceof ShortProcessor) {
            return (short[]) ip.getPixels();
        }
        if (ip instanceof ByteProcessor) {
            byte[] src = (byte[]) ip.getPixels();
            short[] dst = new short[src.length];
            for (int i = 0; i < src.length; ++i) dst[i] = (short) (src[i] & 0xFF);
            return dst;
        }
        throw new IOException("Radelta export currently supports only 8-bit and 16-bit grayscale stacks/hyperstacks.");
    }

    private static void validateSupportedImage(ImagePlus imp) throws IOException {
        if (imp == null) throw new IOException("No image is open.");
        int type = imp.getType();
        if (!(type == ImagePlus.GRAY8 || type == ImagePlus.GRAY16)) {
            throw new IOException("Radelta export currently supports only 8-bit and 16-bit grayscale stacks/hyperstacks.");
        }
        if (imp.getWidth() <= 0 || imp.getHeight() <= 0) {
            throw new IOException("Image dimensions are invalid.");
        }
    }

    private static String suggestedName(ImagePlus imp) {
        String title = imp != null ? imp.getShortTitle() : "image";
        if (title == null || title.trim().isEmpty()) title = "image";
        return title;
    }

    private static void check(int rc, String prefix) throws IOException {
        if (rc == RadeltaNative.OK) return;
        String extra = RadeltaNative.lastError();
        if (extra == null || extra.trim().isEmpty()) throw new IOException(prefix + " (error code " + rc + ")");
        throw new IOException(prefix + ": " + extra + " (error code " + rc + ")");
    }
}
