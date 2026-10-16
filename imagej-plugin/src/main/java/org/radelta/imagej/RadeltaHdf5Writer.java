package org.radelta.imagej;

import ij.IJ;
import ij.ImagePlus;
import ij.ImageStack;
import ij.gui.GenericDialog;
import ij.io.SaveDialog;
import ij.measure.Calibration;

import java.io.IOException;
import java.io.UncheckedIOException;
import java.nio.file.AtomicMoveNotSupportedException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.nio.file.StandardCopyOption;
import java.util.Locale;

/**
 * Writes Radelta-compressed HDF5 datasets through the reader's own HDF5
 * runtime and the bundled embedded filter.
 *
 * <p>Datasets follow the conventions of the {@code radelta-hdf5} tools: uint16
 * in canonical T,C,Z,Y,X order, starting at the first non-singleton axis among
 * T, C and Z, with {@code axes}, {@code radelta_version}, {@code radelta_lossy}
 * and (for lossy data) calibration attributes. {@code element_size_um} records
 * the Z,Y,X spacing in micrometres for Fiji's HDF5 importer.
 */
final class RadeltaHdf5Writer {
    static final String DEFAULT_DATASET = "/images";
    static final int FILTER_ID = 65024;
    static final String EMBEDDED_SUFFIX = ", embedded)";
    private static final long TARGET_CHUNK_BYTES = 8L << 20;
    private static final long MAX_CHUNK_BYTES = (1L << 32) - 1;
    private static final int PARAMETER_VERSION = 1;
    private static final int DEFAULT_BLOCK_DEPTH = 4;
    private static final int CONTEXT_MEAN = 2;
    private static final int DEFAULT_SCALE_BITS = 10;

    /** Encoding settings; chunk extents of zero select the defaults. */
    static final class Settings {
        boolean lossy;
        double offsetAdu = 0.0;
        double gainEPerAdu = 1.0;
        double noiseStep = 2.0;
        int chunkZ;
        int chunkY;
        int chunkX;

        static Settings lossless() {
            return new Settings();
        }

        static Settings lossy(double offsetAdu, double gainEPerAdu, double noiseStep) {
            Settings settings = new Settings();
            settings.lossy = true;
            settings.offsetAdu = offsetAdu;
            settings.gainEPerAdu = gainEPerAdu;
            settings.noiseStep = noiseStep;
            return settings;
        }

        void validate() throws IOException {
            if (lossy && !(Double.isFinite(offsetAdu) && Double.isFinite(gainEPerAdu) && gainEPerAdu > 0.0 &&
                    Double.isFinite(noiseStep) && noiseStep > 0.0)) {
                throw new IOException("Lossy parameters must be finite; gain and noise step must be > 0.");
            }
            if (chunkZ < 0 || chunkY < 0 || chunkX < 0) throw new IOException("Chunk sizes must be positive, or 0 for the default.");
        }
    }

    private RadeltaHdf5Writer() {}

    // Pure layout helpers, unit tested without HDF5.

    /** Dataset shape in HDF5 (slowest-first) order: TCZYX, CZYX, ZYX or YX. */
    static long[] shape(int nx, int ny, int nz, int nc, int nt) {
        if (nt > 1) return new long[] {nt, nc, nz, ny, nx};
        if (nc > 1) return new long[] {nc, nz, ny, nx};
        if (nz > 1) return new long[] {nz, ny, nx};
        return new long[] {ny, nx};
    }

    /** The importer layout and {@code axes} attribute for a dataset rank. */
    static String axes(int rank) {
        return "TCZYX".substring(5 - rank);
    }

    /**
     * Default chunks, as in {@code radelta_hdf5.default_chunks}: up to
     * 1024x1024 in XY and 4 planes in Z, one T/C index, then halve the largest
     * spatial extent until the chunk holds at most 8 MiB of pixels.
     */
    static long[] defaultChunks(long[] shape) {
        int rank = shape.length;
        long[] chunks = new long[rank];
        java.util.Arrays.fill(chunks, 1);
        chunks[rank - 1] = Math.min(shape[rank - 1], 1024);
        chunks[rank - 2] = Math.min(shape[rank - 2], 1024);
        if (rank >= 3) chunks[rank - 3] = Math.min(shape[rank - 3], 4);
        while (product(chunks) * 2 > TARGET_CHUNK_BYTES) {
            int axis = Math.max(0, rank - 3);
            for (int i = axis + 1; i < rank; ++i) if (chunks[i] > chunks[axis]) axis = i;
            chunks[axis] = (chunks[axis] + 1) / 2;
        }
        return chunks;
    }

    /** Chunks with any requested Z/Y/X extents (clamped to the shape) applied. */
    static long[] chunks(long[] shape, Settings settings) throws IOException {
        long[] chunks = defaultChunks(shape);
        int rank = shape.length;
        int[] requested = {settings.chunkZ, settings.chunkY, settings.chunkX};
        for (int i = 0; i < 3; ++i) {
            int axis = rank - 3 + i;
            if (axis >= 0 && requested[i] > 0) chunks[axis] = Math.min(shape[axis], requested[i]);
        }
        if (product(chunks) * 2 > MAX_CHUNK_BYTES) throw new IOException("An HDF5 chunk must be smaller than 4 GiB.");
        return chunks;
    }

    /**
     * All 17 filter parameters. The embedded filter cannot derive the chunk
     * geometry and byte order itself, so the writer supplies them: version,
     * lossy flag, block depth, context, scale bits, offset/gain/noise as
     * low/high words, chunk X,Y,Z,C,T padded with 1, and 0 for little-endian.
     */
    static int[] filterParameters(Settings settings, long[] chunks) {
        int[] p = new int[17];
        p[0] = PARAMETER_VERSION;
        p[1] = settings.lossy ? 1 : 0;
        p[2] = DEFAULT_BLOCK_DEPTH;
        p[3] = CONTEXT_MEAN;
        p[4] = DEFAULT_SCALE_BITS;
        double[] values = {settings.offsetAdu, settings.gainEPerAdu, settings.noiseStep};
        for (int i = 0; i < 3; ++i) {
            long bits = Double.doubleToRawLongBits(values[i]);
            p[5 + 2 * i] = (int) bits;
            p[6 + 2 * i] = (int) (bits >>> 32);
        }
        for (int i = 0; i < 5; ++i) p[11 + i] = 1;
        for (int i = 0; i < chunks.length; ++i) p[11 + i] = (int) chunks[chunks.length - 1 - i];
        p[16] = 0;
        return p;
    }

    /** Micrometres per unit, or NaN when a calibration unit is not a length we know. */
    static double micrometresPer(String unit) {
        if (unit == null) return Double.NaN;
        switch (unit.trim().toLowerCase(Locale.ROOT)) {
            case "nm": case "nanometer": case "nanometre": case "nanometers": case "nanometres":
                return 1e-3;
            case "um": case "µm": case "μm": case "micron": case "microns":
            case "micrometer": case "micrometre": case "micrometers": case "micrometres":
                return 1.0;
            case "mm": case "millimeter": case "millimetre": case "millimeters": case "millimetres":
                return 1e3;
            case "cm": case "centimeter": case "centimetre": case "centimeters": case "centimetres":
                return 1e4;
            case "m": case "meter": case "metre": case "meters": case "metres":
                return 1e6;
            default:
                return Double.NaN;
        }
    }

    /** Z,Y,X spacing in micrometres, or null when the image has no length calibration. */
    static float[] elementSizeUm(Calibration calibration) {
        if (calibration == null) return null;
        double z = calibration.pixelDepth * micrometresPer(calibration.getZUnit());
        double y = calibration.pixelHeight * micrometresPer(calibration.getYUnit());
        double x = calibration.pixelWidth * micrometresPer(calibration.getXUnit());
        if (!(z > 0 && y > 0 && x > 0) || Double.isInfinite(z + y + x)) return null;
        return new float[] {(float) z, (float) y, (float) x};
    }

    /** The codec version in the embedded filter's name, e.g. "0.8.0". */
    static String filterVersion(String name) {
        String prefix = "Radelta ";
        if (name == null || !name.startsWith(prefix)) return "unknown";
        int end = name.indexOf(' ', prefix.length());
        return end < 0 ? "unknown" : name.substring(prefix.length(), end);
    }

    static String normalizeDataset(String dataset) throws IOException {
        String name = dataset == null ? "" : dataset.trim();
        if (name.isEmpty()) name = DEFAULT_DATASET;
        if (!name.startsWith("/")) name = "/" + name;
        if (name.endsWith("/") || name.contains("//") || name.contains("/./") || name.contains("/../"))
            throw new IOException("Invalid HDF5 dataset name: " + dataset);
        return name;
    }

    private static long product(long[] values) {
        long result = 1;
        for (long value : values) result *= value;
        return result;
    }

    // Writing.

    static void write(ImagePlus imp, String filename, String dataset, Settings settings) throws IOException {
        RadeltaExporter.validateSupportedImage(imp);
        settings.validate();
        if (filename == null || filename.trim().isEmpty()) throw new IOException("No output file was given.");
        String datasetName = normalizeDataset(dataset);
        int nx = imp.getWidth(), ny = imp.getHeight();
        int nz = Math.max(1, imp.getNSlices()), nc = Math.max(1, imp.getNChannels()), nt = Math.max(1, imp.getNFrames());
        long[] shape = shape(nx, ny, nz, nc, nt);
        long[] chunks = chunks(shape, settings);
        // Before any HDF5 work: replacing a virtual stack's own source file (or
        // a hard link to it) would pull the file out from under the stack.
        RadeltaExporter.validateDestination(imp, filename);

        RadeltaHdf5.configure();
        Hdf5Bindings h5 = new Hdf5Bindings();
        String filterName = h5.filterName(FILTER_ID);
        if (!filterName.endsWith(EMBEDDED_SUFFIX)) {
            throw new IOException("The registered Radelta HDF5 filter (" + filterName + ") cannot write from Fiji. " +
                    "Clear any HDF5_PLUGIN_PATH setting and restart Fiji.");
        }

        Path target = Paths.get(filename).toAbsolutePath();
        Path directory = target.getParent();
        if (directory == null || !Files.isDirectory(directory)) throw new IOException("Output folder does not exist: " + directory);
        // Write beside the target, then replace it only after a successful close.
        // The prefix matches the native writers' temporary files.
        Path temporary = Files.createTempFile(directory, ".radelta-", ".tmp");
        boolean finished = false;
        try {
            long file = h5.createFile(temporary.toString());
            try {
                writeDataset(h5, file, imp, datasetName, shape, chunks, settings, filterVersion(filterName));
            } finally {
                h5.closeFile(file);
            }
            try {
                Files.move(temporary, target, StandardCopyOption.REPLACE_EXISTING, StandardCopyOption.ATOMIC_MOVE);
            } catch (AtomicMoveNotSupportedException e) {
                Files.move(temporary, target, StandardCopyOption.REPLACE_EXISTING);
            }
            finished = true;
        } finally {
            if (!finished) Files.deleteIfExists(temporary);
            IJ.showProgress(1.0);
        }
        IJ.showStatus("Wrote " + (settings.lossy ? "lossy" : "lossless") + " Radelta HDF5: " + target);
    }

    private static void writeDataset(Hdf5Bindings h5, long file, ImagePlus imp, String name, long[] shape, long[] chunks,
                                     Settings settings, String version) throws IOException {
        long space = h5.createSimpleSpace(shape);
        try {
            long lcpl = h5.createPropertyList("H5P_LINK_CREATE");
            long dcpl = -1;
            long dataset = -1;
            try {
                h5.createIntermediateGroups(lcpl);
                dcpl = h5.createPropertyList("H5P_DATASET_CREATE");
                h5.setChunk(dcpl, chunks);
                h5.setFilter(dcpl, FILTER_ID, filterParameters(settings, chunks));
                dataset = h5.createDataset(file, name, h5.constant("H5T_STD_U16LE"), space, lcpl, dcpl);
                writePixels(h5, dataset, space, imp, shape, chunks);
                writeAttributes(h5, dataset, imp, shape.length, settings, version);
            } finally {
                if (dataset >= 0) h5.closeDataset(dataset);
                if (dcpl >= 0) h5.closePropertyList(dcpl);
                h5.closePropertyList(lcpl);
            }
        } finally {
            h5.closeSpace(space);
        }
    }

    /**
     * Write one chunk at a time, so memory use is bounded by the chunk size
     * and each HDF5 write passes whole chunks to the filter.
     */
    private static void writePixels(Hdf5Bindings h5, long dataset, long fileSpace, ImagePlus imp,
                                    long[] shape, long[] chunks) throws IOException {
        int rank = shape.length;
        // Pad to T,C,Z,Y,X so one loop serves every rank.
        long[] size = {1, 1, 1, 1, 1}, chunk = {1, 1, 1, 1, 1};
        for (int i = 0; i < rank; ++i) {
            size[5 - rank + i] = shape[i];
            chunk[5 - rank + i] = chunks[i];
        }
        long[] counts = new long[5];
        long total = 1;
        for (int i = 0; i < 5; ++i) {
            counts[i] = (size[i] + chunk[i] - 1) / chunk[i];
            total *= counts[i];
        }
        ImageStack stack = imp.getStack();
        long done = 0;
        short[] buffer = new short[(int) (chunk[2] * chunk[3] * chunk[4])];
        for (int t = 0; t < size[0]; ++t) {
            for (int c = 0; c < size[1]; ++c) {
                for (long z0 = 0; z0 < size[2]; z0 += chunk[2]) {
                    int dz = (int) Math.min(chunk[2], size[2] - z0);
                    short[][] planes = new short[dz][];
                    for (int k = 0; k < dz; ++k) {
                        try {
                            planes[k] = RadeltaExporter.extractPlaneU16(stack, imp.getStackIndex(c + 1, (int) z0 + k + 1, t + 1));
                        } catch (UncheckedIOException e) {
                            // A virtual stack that cannot decode a plane must abort
                            // the save rather than export a damaged image.
                            throw new IOException("Failed while reading source plane C=" + (c + 1) + ", Z=" + (z0 + k + 1) +
                                    ", T=" + (t + 1), e.getCause());
                        }
                    }
                    for (long y0 = 0; y0 < size[3]; y0 += chunk[3]) {
                        int dy = (int) Math.min(chunk[3], size[3] - y0);
                        for (long x0 = 0; x0 < size[4]; x0 += chunk[4]) {
                            int dx = (int) Math.min(chunk[4], size[4] - x0);
                            int n = 0;
                            for (int k = 0; k < dz; ++k)
                                for (int y = 0; y < dy; ++y) {
                                    System.arraycopy(planes[k], (int) ((y0 + y) * size[4] + x0), buffer, n, dx);
                                    n += dx;
                                }
                            long[] start = {t, c, z0, y0, x0}, count = {1, 1, dz, dy, dx};
                            h5.selectHyperslab(fileSpace, tail(start, rank), tail(count, rank));
                            // A memory space shaped like the selection keeps HDF5
                            // 1.10 on its fast path; a flat one is 2.5x slower.
                            long memory = h5.createSimpleSpace(tail(count, rank));
                            try {
                                h5.writeUint16(dataset, memory, fileSpace, n == buffer.length ? buffer : java.util.Arrays.copyOf(buffer, n));
                            } finally {
                                h5.closeSpace(memory);
                            }
                            done++;
                            IJ.showProgress((double) done / total);
                        }
                    }
                    IJ.showStatus("Radelta HDF5 export: " + done + "/" + total + " chunks");
                }
            }
        }
    }

    private static long[] tail(long[] values, int rank) {
        return java.util.Arrays.copyOfRange(values, values.length - rank, values.length);
    }

    private static void writeAttributes(Hdf5Bindings h5, long dataset, ImagePlus imp, int rank, Settings settings,
                                        String version) throws IOException {
        h5.writeStringAttribute(dataset, "axes", axes(rank));
        h5.writeStringAttribute(dataset, "radelta_version", version);
        h5.writeUint8Attribute(dataset, "radelta_lossy", settings.lossy ? 1 : 0);
        if (settings.lossy) {
            h5.writeDoubleAttribute(dataset, "offset_adu", settings.offsetAdu);
            h5.writeDoubleAttribute(dataset, "gain_e_per_adu", settings.gainEPerAdu);
            h5.writeDoubleAttribute(dataset, "noise_step", settings.noiseStep);
        }
        float[] spacing = elementSizeUm(imp.getCalibration());
        if (spacing != null) h5.writeFloatArrayAttribute(dataset, "element_size_um", spacing);
    }

    // Dialogs.

    static void saveInteractive(ImagePlus imp, boolean lossy) throws IOException {
        RadeltaExporter.validateSupportedImage(imp);
        RadeltaHdf5.configure();
        String title = lossy ? "Save as Radelta HDF5 (lossy)" : "Save as Radelta HDF5 (lossless)";
        String suggested = imp.getShortTitle();
        if (suggested == null || suggested.trim().isEmpty()) suggested = "image";
        SaveDialog sd = new SaveDialog(title + "...", suggested, ".h5");
        if (sd.getFileName() == null) return;
        long[] shape = shape(imp.getWidth(), imp.getHeight(), Math.max(1, imp.getNSlices()),
                Math.max(1, imp.getNChannels()), Math.max(1, imp.getNFrames()));
        long[] defaults = defaultChunks(shape);
        int rank = shape.length;

        GenericDialog gd = new GenericDialog(title);
        gd.addStringField("Output", sd.getDirectory() + sd.getFileName(), 50);
        gd.addStringField("Dataset", DEFAULT_DATASET, 20);
        if (rank >= 3) gd.addNumericField("Z chunk", defaults[rank - 3], 0);
        gd.addNumericField("Y chunk", defaults[rank - 2], 0);
        gd.addNumericField("X chunk", defaults[rank - 1], 0);
        if (lossy) {
            gd.addNumericField("Offset (ADU)", 0.0, 4);
            gd.addNumericField("Gain", 1.0, 6);
            gd.addChoice("Gain units", new String[] {"e-/ADU", "ADU/e-"}, "e-/ADU");
            gd.addNumericField("Noise step", 2.0, 4);
        }
        gd.addMessage("Layout for Fiji's HDF5 importer: " + axes(rank).toLowerCase(Locale.ROOT));
        gd.showDialog();
        if (gd.wasCanceled()) return;

        String output = gd.getNextString();
        String dataset = gd.getNextString();
        Settings settings = new Settings();
        settings.chunkZ = rank >= 3 ? positive(gd.getNextNumber(), "Z chunk") : 0;
        settings.chunkY = positive(gd.getNextNumber(), "Y chunk");
        settings.chunkX = positive(gd.getNextNumber(), "X chunk");
        if (lossy) {
            settings.lossy = true;
            settings.offsetAdu = gd.getNextNumber();
            double gain = gd.getNextNumber();
            if ("ADU/e-".equals(gd.getNextChoice())) {
                if (gain == 0.0) throw new IOException("Gain in ADU/e- must be non-zero.");
                gain = 1.0 / gain;
            }
            settings.gainEPerAdu = gain;
            settings.noiseStep = gd.getNextNumber();
        }
        write(imp, output, dataset, settings);
    }

    private static int positive(double value, String label) throws IOException {
        if (!(value >= 1 && value <= Integer.MAX_VALUE) || value != Math.rint(value))
            throw new IOException(label + " must be a positive whole number.");
        return (int) value;
    }
}
