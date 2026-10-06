package org.radelta.imagej;

import ij.CompositeImage;
import ij.IJ;
import ij.ImageListener;
import ij.ImagePlus;
import ij.ImageStack;
import ij.gui.GenericDialog;

import java.io.File;
import java.io.IOException;
import java.math.BigInteger;
import java.util.Properties;

/** Shared ImagePlus construction used by menu, File>Open and drag/drop. */
public final class RadeltaImageFactory {
    private static final String MODE_VIRTUAL = "Virtual stack (decode planes on demand)";
    private static final String MODE_MEMORY = "Load entire dataset into RAM";

    private RadeltaImageFactory() {}

    /**
     * Opens a Radelta file interactively. Metadata are read first, then the
     * user chooses virtual-stack or full-RAM loading after seeing the required
     * uncompressed pixel memory. Returns null if the dialog is cancelled.
     */
    public static ImagePlus openForDisplay(String path) throws IOException {
        return openForDisplay(path, rf -> java.awt.GraphicsEnvironment.isHeadless()
                ? OpenMode.VIRTUAL : chooseOpenMode(rf));
    }

    /** Non-interactive opening for scripts and headless callers. */
    public static ImagePlus openForDisplay(String path, boolean loadIntoMemory) throws IOException {
        return openForDisplay(path, rf -> loadIntoMemory ? OpenMode.MEMORY : OpenMode.VIRTUAL);
    }

    interface ModeChooser {
        OpenMode choose(RadeltaFile file);
    }

    static ImagePlus openForDisplay(String path, ModeChooser chooser) throws IOException {
        final RadeltaFile rf = RadeltaFile.open(path);
        boolean transferred = false;
        try {
            final OpenMode mode = chooser.choose(rf);
            if (mode == null) return null;
            final ImagePlus base = mode == OpenMode.MEMORY
                    ? openInMemory(path, rf) : openVirtual(path, rf);
            ImagePlus shown = base;
            if (base.getNChannels() > 1) {
                shown = new CompositeImage(base, CompositeImage.COMPOSITE);
                copyProperties(shown, base);
            }
            if (mode == OpenMode.VIRTUAL) {
                closeWithImage((RadeltaVirtualStack) base.getStack());
                transferred = true;
            }
            return shown;
        } finally {
            if (!transferred) rf.close();
        }
    }

    /** Non-interactive virtual-stack opening retained for programmatic callers. */
    public static ImagePlus openBase(String path) throws IOException {
        final RadeltaFile rf = RadeltaFile.open(path);
        boolean transferred = false;
        try {
            ImagePlus base = openVirtual(path, rf);
            closeWithImage((RadeltaVirtualStack) base.getStack());
            transferred = true;
            return base;
        } finally {
            if (!transferred) rf.close();
        }
    }

    enum OpenMode { VIRTUAL, MEMORY }

    private static OpenMode chooseOpenMode(RadeltaFile rf) {
        final BigInteger pixelBytes = rf.pixelBytesBig();
        final Runtime runtime = Runtime.getRuntime();
        final long maxHeap = runtime.maxMemory();
        final long usedHeap = runtime.totalMemory() - runtime.freeMemory();
        final long heapHeadroom = Math.max(0L, maxHeap - usedHeap);

        final GenericDialog gd = new GenericDialog("Open Radelta");
        gd.addMessage(
                "Dataset: " + rf.width + " x " + rf.height +
                ", C=" + rf.channels + ", Z=" + rf.slices + ", T=" + rf.frames + "\n" +
                "Format: " + formatName(rf.format) + (rf.lossy ? " (lossy)" : " (lossless)") + "\n\n" +
                "Minimum pixel memory for a fully loaded 16-bit volume: " + formatBytes(pixelBytes) + "\n" +
                "Current Java heap limit: " + formatBytes(BigInteger.valueOf(maxHeap)) + "\n" +
                "Approx. unused Java heap now: " + formatBytes(BigInteger.valueOf(heapHeadroom)) + "\n\n" +
                "A full load also needs additional Java/ImageJ and Radelta decoding overhead.\n" +
                "A virtual stack keeps the dataset on disk and decodes planes on demand."
        );

        if (pixelBytes.compareTo(BigInteger.valueOf(heapHeadroom)) >= 0) {
            gd.addMessage(
                    "WARNING: the pixel data alone are at least as large as the currently unused Java heap.\n" +
                    "Loading the full dataset is likely to fail unless Fiji's memory limit is increased."
            );
        }

        gd.addChoice("Open as", new String[] { MODE_VIRTUAL, MODE_MEMORY }, MODE_VIRTUAL);
        gd.showDialog();
        if (gd.wasCanceled()) return null;
        return MODE_MEMORY.equals(gd.getNextChoice()) ? OpenMode.MEMORY : OpenMode.VIRTUAL;
    }

    private static ImagePlus openVirtual(String path, final RadeltaFile rf) throws IOException {
        final RadeltaVirtualStack stack = new RadeltaVirtualStack(rf);
        final ImagePlus base = new ImagePlus(new File(path).getName(), stack);
        configure(base, rf, path, "virtual");
        return base;
    }

    private static void closeWithImage(final RadeltaVirtualStack stack) {
        final ImageListener listener = new ImageListener() {
            private boolean closed = false;

            @Override public void imageOpened(ImagePlus imp) {}
            @Override public void imageUpdated(ImagePlus imp) {}

            @Override public void imageClosed(ImagePlus imp) {
                if (!closed && imp != null && imp.getStack() == stack) {
                    closed = true;
                    stack.close();
                    ImagePlus.removeImageListener(this);
                }
            }
        };
        ImagePlus.addImageListener(listener);
    }

    private static ImagePlus openInMemory(String path, RadeltaFile rf) throws IOException {
        final ImageStack stack = new ImageStack(rf.width, rf.height);
        final long totalPlanes = (long) rf.channels * (long) rf.slices * (long) rf.frames;
        long done = 0;

        try {
            // ImageJ hyperstack order is C fastest, then Z, then T.
            for (int t = 0; t < rf.frames; ++t) {
                for (int z = 0; z < rf.slices; ++z) {
                    for (int c = 0; c < rf.channels; ++c) {
                        final short[] pixels = rf.readPlane(t, c, z);
                        stack.addSlice("C=" + (c + 1) + " Z=" + (z + 1) + " T=" + (t + 1), pixels);
                        done++;
                        IJ.showProgress((double) done / (double) totalPlanes);
                        IJ.showStatus("Loading Radelta into RAM: " + done + "/" + totalPlanes + " planes");
                    }
                }
            }
        } catch (OutOfMemoryError oom) {
            throw new IOException(
                    "Not enough Java heap to load the complete Radelta dataset. " +
                    "Reopen it as a virtual stack or increase Fiji's memory limit.", oom);
        } finally {
            IJ.showProgress(1.0);
        }

        final ImagePlus base = new ImagePlus(new File(path).getName(), stack);
        configure(base, rf, path, "memory");
        return base;
    }

    private static void configure(ImagePlus base, RadeltaFile rf, String path, String mode) throws IOException {
        base.setDimensions(rf.channels, rf.slices, rf.frames);
        base.setOpenAsHyperStack(rf.channels > 1 || rf.frames > 1);
        RadeltaMetadata.apply(base, rf.readMetadata());
        base.setProperty("Radelta.Format", formatName(rf.format));
        base.setProperty("Radelta.Lossy", Boolean.toString(rf.lossy));
        base.setProperty("Radelta.Streaming", Boolean.toString(rf.streaming));
        base.setProperty("Radelta.Source", path);
        base.setProperty("Radelta.OpenMode", mode);
    }

    /** Copy the image payload/metadata needed by HandleExtraFileTypes into
     * the plugin instance ImagePlus that ImageJ will receive. */
    public static void adoptInto(ImagePlus target, ImagePlus source) {
        target.setStack(source.getTitle(), source.getStack());
        target.setDimensions(source.getNChannels(), source.getNSlices(), source.getNFrames());
        target.setOpenAsHyperStack(source.getOpenAsHyperStack());
        target.setCalibration(source.getCalibration());
        if (source.getOriginalFileInfo() != null) {
            target.setFileInfo(source.getOriginalFileInfo());
        }

        copyProperties(target, source);
    }

    private static void copyProperties(ImagePlus target, ImagePlus source) {
        String[] imageProperties = source.getPropertiesAsArray();
        if (imageProperties != null) target.setProperties(imageProperties);
        Properties properties = source.getProperties();
        if (properties != null) {
            for (Object key : properties.keySet()) {
                Object value = properties.get(key);
                if (key != null && value != null) {
                    target.setProperty(key.toString(), value);
                }
            }
        }
    }

    public static boolean isRadeltaPath(String path) {
        if (path == null) return false;
        String lower = path.toLowerCase(java.util.Locale.ROOT);
        return lower.endsWith(".rdlt") || lower.endsWith(".radelta");
    }

    public static String formatName(int f) {
        switch (f) {
            case 1: return "RDL1";
            case 2: return "RDLQ";
            case 3: return "RDM2";
            case 4: return "RDQ2";
            case 5: return "RDS3";
            case 6: return "RQS3";
            default: return "unknown(" + f + ")";
        }
    }

    private static String formatBytes(BigInteger bytes) {
        final BigInteger kib = BigInteger.valueOf(1024L);
        final BigInteger mib = kib.multiply(kib);
        final BigInteger gib = mib.multiply(kib);
        final BigInteger tib = gib.multiply(kib);
        if (bytes.compareTo(tib) >= 0) return formatBinary(bytes, tib, "TiB");
        if (bytes.compareTo(gib) >= 0) return formatBinary(bytes, gib, "GiB");
        if (bytes.compareTo(mib) >= 0) return formatBinary(bytes, mib, "MiB");
        if (bytes.compareTo(kib) >= 0) return formatBinary(bytes, kib, "KiB");
        return bytes.toString() + " bytes";
    }

    private static String formatBinary(BigInteger bytes, BigInteger unit, String suffix) {
        // Integer arithmetic gives a stable one-decimal display without overflow.
        BigInteger tenths = bytes.multiply(BigInteger.TEN).divide(unit);
        BigInteger whole = tenths.divide(BigInteger.TEN);
        BigInteger frac = tenths.remainder(BigInteger.TEN);
        return whole.toString() + "." + frac.toString() + " " + suffix;
    }
}
