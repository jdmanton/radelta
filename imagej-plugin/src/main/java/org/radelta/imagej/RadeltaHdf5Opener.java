package org.radelta.imagej;

import ij.ImagePlus;
import ij.gui.GenericDialog;

import java.io.File;
import java.io.IOException;
import java.util.ArrayList;
import java.util.HashSet;
import java.util.List;
import java.util.Locale;
import java.util.Set;

/**
 * Opens HDF5 files containing Radelta-compressed datasets from File &gt; Open
 * and drag-and-drop, which would otherwise reach Bio-Formats.
 *
 * <p>A file with one Radelta dataset whose {@code axes} attribute gives its
 * layout (as written by the plugin and the {@code radelta-hdf5} tools) opens
 * directly. Otherwise a dialog chooses the dataset and layout.
 */
final class RadeltaHdf5Opener {
    /** Bounds the walk of pathological files; real images have few datasets. */
    private static final int MAX_OBJECTS = 100000;

    static final class Dataset {
        final String path;
        final long[] shape;
        /** The dataset's {@code axes} attribute, or null. */
        final String axes;

        Dataset(String path, long[] shape, String axes) {
            this.path = path;
            this.shape = shape;
            this.axes = axes;
        }

        /** The importer layout recorded in the file, or null if absent or invalid. */
        String recordedLayout() {
            return validLayout(axes, shape.length);
        }

        String layout() {
            String recorded = recordedLayout();
            return recorded != null ? recorded : defaultLayout(shape.length);
        }

        String label() {
            StringBuilder size = new StringBuilder();
            for (int i = 0; i < shape.length; ++i) size.append(i == 0 ? "" : "x").append(shape[i]);
            return path + " (" + size + ")";
        }
    }

    private RadeltaHdf5Opener() {}

    static boolean hasHdf5Extension(String path) {
        String name = path == null ? "" : path.toLowerCase(Locale.ROOT);
        return name.endsWith(".h5") || name.endsWith(".hdf5") || name.endsWith(".he5");
    }

    /** Canonical Radelta layout for a rank, as written by the plugin and tools. */
    static String defaultLayout(int rank) {
        return rank >= 2 && rank <= 5 ? "tczyx".substring(5 - rank) : null;
    }

    /**
     * A lower-case importer layout if {@code axes} names each of rank distinct
     * axes from T, C, Z, Y and X and includes Y and X; otherwise null.
     */
    static String validLayout(String axes, int rank) {
        if (axes == null) return null;
        String layout = axes.trim().toLowerCase(Locale.ROOT);
        if (layout.length() != rank || layout.indexOf('y') < 0 || layout.indexOf('x') < 0) return null;
        for (int i = 0; i < layout.length(); ++i) {
            char axis = layout.charAt(i);
            if ("tczyx".indexOf(axis) < 0 || layout.indexOf(axis) != i) return null;
        }
        return layout;
    }

    /** Radelta-filtered datasets of rank 2 to 5, in name order. */
    static List<Dataset> radeltaDatasets(String path) throws IOException {
        return radeltaDatasets(path, Integer.MAX_VALUE);
    }

    /** As above, stopping once {@code limit} datasets are found. */
    static List<Dataset> radeltaDatasets(String path, int limit) throws IOException {
        Hdf5Bindings h5 = new Hdf5Bindings();
        List<Dataset> found = new ArrayList<Dataset>();
        long file = h5.openFileReadOnly(path);
        try {
            int[] budget = {MAX_OBJECTS};
            collect(h5, file, "/", found, new HashSet<Long>(), budget, limit);
        } finally {
            h5.closeFile(file);
        }
        return found;
    }

    private static void collect(Hdf5Bindings h5, long file, String group, List<Dataset> found,
                                Set<Long> visited, int[] budget, int limit) throws IOException {
        long links = h5.linkCount(file, group);
        int groupType = h5.intConstant("H5O_TYPE_GROUP"), datasetType = h5.intConstant("H5O_TYPE_DATASET");
        for (long i = 0; i < links && budget[0] > 0 && found.size() < limit; ++i, --budget[0]) {
            String path;
            long[] info;
            try {
                path = (group.equals("/") ? "/" : group + "/") + h5.linkName(file, group, i);
                info = h5.objectTypeAndAddress(file, path);
            } catch (IOException e) {
                continue; // dangling or unreadable link: not an image we can open
            }
            if (!visited.add(info[1])) continue; // hard links can form cycles
            if (info[0] == groupType) {
                collect(h5, file, path, found, visited, budget, limit);
            } else if (info[0] == datasetType) {
                try {
                    long dataset = h5.openDataset(file, path);
                    try {
                        long[] shape = h5.datasetShape(dataset);
                        boolean radelta = false;
                        for (int filter : h5.datasetFilters(dataset)) radelta |= filter == RadeltaHdf5Writer.FILTER_ID;
                        if (radelta && shape.length >= 2 && shape.length <= 5)
                            found.add(new Dataset(path, shape, h5.readStringAttribute(file, path, "axes")));
                    } finally {
                        h5.closeDataset(dataset);
                    }
                } catch (IOException e) {
                    // Skip datasets HDF5 cannot describe; others may still open.
                }
            }
        }
    }

    /**
     * True for HDF5 files with at least one Radelta dataset, when Fiji's HDF5
     * reader is installed. Never throws: other readers must still get the file.
     */
    static boolean supports(String path) {
        try {
            if (!hasHdf5Extension(path) || !RadeltaHdf5.hasReader() || !new File(path).isFile()) return false;
            if (!new Hdf5Bindings().isHdf5(path)) return false;
            // Runs for every HDF5 file opened in Fiji: stop at the first match.
            return !radeltaDatasets(path, 1).isEmpty();
        } catch (IOException | RuntimeException | LinkageError e) {
            return false;
        }
    }

    /** Open a Radelta dataset from a file, asking which one when that is ambiguous; null if cancelled. */
    static ImagePlus open(String path) throws IOException {
        RadeltaHdf5.configure();
        List<Dataset> datasets = radeltaDatasets(path);
        if (datasets.isEmpty()) throw new IOException("No Radelta-compressed datasets in " + path);
        Dataset chosen = datasets.get(0);
        String layout = chosen.recordedLayout();
        if (datasets.size() > 1 || layout == null) {
            GenericDialog gd = new GenericDialog("Open Radelta HDF5");
            String[] labels = new String[datasets.size()];
            for (int i = 0; i < labels.length; ++i) labels[i] = datasets.get(i).label();
            gd.addChoice("Dataset", labels, labels[0]);
            gd.addStringField("Layout", datasets.size() == 1 ? chosen.layout() : "", 8);
            gd.addMessage("Layout lists the dataset's axes, slowest first, e.g. zyx or tczyx.\n" +
                    "Leave it blank to use the dataset's axes attribute, or yx/zyx/czyx/tczyx by rank.");
            gd.showDialog();
            if (gd.wasCanceled()) return null;
            chosen = datasets.get(gd.getNextChoiceIndex());
            String requested = gd.getNextString().trim();
            layout = requested.isEmpty() ? chosen.layout() : validLayout(requested, chosen.shape.length);
            if (layout == null) {
                throw new IOException("Layout \"" + requested + "\" must name " + chosen.shape.length +
                        " distinct axes from t, c, z, y and x, including y and x.");
            }
        }
        return RadeltaHdf5.open(path, chosen.path, layout);
    }
}
