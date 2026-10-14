package org.radelta.imagej;

import ij.ImagePlus;
import java.io.IOException;
import java.io.InputStream;
import java.lang.reflect.InvocationTargetException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.util.Locale;

/** Register our bundled filter in the reader's own HDF5 runtime, and read or write through it. */
public final class RadeltaHdf5 {
    private static Path configuredDirectory;

    static Class<?> readerClass(String name) throws ClassNotFoundException {
        return Class.forName(name, true, RadeltaHdf5.class.getClassLoader());
    }

    static boolean hasReader() {
        try {
            Class.forName("hdf.hdf5lib.H5", false, RadeltaHdf5.class.getClassLoader());
            return true;
        } catch (ClassNotFoundException e) {
            return false;
        } catch (LinkageError e) {
            return true; // configure() reports the actual runtime error safely.
        }
    }

    /** No environment-variable changes, JNI registration or second HDF5 SDK. */
    public static synchronized void configure() throws IOException {
        if (configuredDirectory != null) return;
        try {
            Class<?> h5 = readerClass("hdf.hdf5lib.H5");
            Path directory;
            String override = System.getProperty("radelta.hdf5.plugin.path");
            if (override != null && !override.trim().isEmpty()) {
                directory = Paths.get(override).toAbsolutePath();
                if (!Files.isDirectory(directory)) throw new IOException("HDF5 decoder directory does not exist: " + directory);
            } else {
                String resource = nativeResource(System.getProperty("os.name", ""), System.getProperty("os.arch", ""));
                if (resource == null) throw new IOException("No bundled HDF5 decoder for this Java runtime's operating system/architecture");
                try (InputStream input = RadeltaHdf5.class.getResourceAsStream(resource)) {
                    if (input == null) throw new IOException("Bundled HDF5 decoder is missing. Install the complete Radelta Fiji release bundle.");
                    directory = Files.createTempDirectory("radelta-imagej-hdf5-");
                    directory.toFile().deleteOnExit();
                    Path library = directory.resolve(resource.substring(resource.lastIndexOf('/') + 1));
                    Files.copy(input, library);
                    library.toFile().deleteOnExit();
                }
            }
            h5.getMethod("H5PLprepend", String.class).invoke(null, directory.toString());
            int available = ((Number)h5.getMethod("H5Zfilter_avail", int.class).invoke(null, 65024)).intValue();
            if (available <= 0) throw new IOException("HDF5 could not load the bundled Radelta decoder");
            int flags = ((Number)h5.getMethod("H5Zget_filter_info", int.class).invoke(null, 65024)).intValue();
            if ((flags & 2) == 0) throw new IOException("The registered Radelta filter cannot decode images");
            // The SDK filter links a second HDF5 runtime and older decoders cannot
            // read checksummed data; only the bundled embedded build is safe here.
            String name = new Hdf5Bindings().filterName(65024);
            if (!name.endsWith(RadeltaHdf5Writer.EMBEDDED_SUFFIX))
                throw new IOException("Another Radelta HDF5 filter (" + name + ") is already registered. " +
                    "Clear any HDF5_PLUGIN_PATH setting and restart Fiji.");
            configuredDirectory = directory;
        } catch (ClassNotFoundException e) {
            throw new IOException("The HDF5 reader is missing. Install the Radelta Fiji bundle and restart Fiji.", e);
        } catch (ReflectiveOperationException | LinkageError e) {
            Throwable cause = e instanceof InvocationTargetException ? ((InvocationTargetException)e).getCause() : e;
            String os = System.getProperty("os.name", "").toLowerCase(Locale.ROOT);
            String arch = System.getProperty("os.arch", "").toLowerCase(Locale.ROOT);
            String remedy = os.startsWith("linux") && (arch.equals("aarch64") || arch.equals("arm64")) ?
                "JHDF5 19.04.1 has no Linux ARM64 native runtime; install a compatible JHDF5 build and restart Fiji." :
                "Use the bundled JHDF5 19.04.1 reader for your Java architecture.";
            throw new IOException("Cannot initialise Fiji HDF5 support: " + cause + ". " + remedy, cause);
        }
    }

    /** Scriptable headless import using the existing HDF5_Vibez implementation. */
    public static ImagePlus open(String filename, String dataset, String layout) throws IOException {
        configure();
        try {
            Object image = readerClass("sc.fiji.hdf5.HDF5ImageJ").getMethod("hdf5read", String.class, String.class, String.class)
                .invoke(null, filename, dataset, layout);
            if (!(image instanceof ImagePlus)) throw new IOException("HDF5 import failed for " + dataset);
            return (ImagePlus)image;
        } catch (ReflectiveOperationException | LinkageError e) {
            Throwable cause = e instanceof InvocationTargetException ? ((InvocationTargetException)e).getCause() : e;
            throw new IOException("Cannot open the HDF5 dataset: " + cause, cause);
        }
    }

    /**
     * Open a file's Radelta-compressed dataset as File &gt; Open and
     * drag-and-drop do: directly when the file has one such dataset with an
     * {@code axes} attribute, otherwise after asking for the dataset and
     * layout. Returns null if that dialog is cancelled.
     */
    public static ImagePlus openFile(String filename) throws IOException {
        return RadeltaHdf5Opener.open(filename);
    }

    /**
     * Save a grayscale image as a lossless Radelta HDF5 dataset, replacing
     * {@code filename}. Hyperstacks use canonical TCZYX/CZYX/ZYX/YX axes; see
     * {@link #save(ImagePlus, String, String, boolean, double, double, double, int, int, int)}.
     */
    public static void save(ImagePlus image, String filename, String dataset) throws IOException {
        RadeltaHdf5Writer.write(image, filename, dataset, RadeltaHdf5Writer.Settings.lossless());
    }

    /** Save with camera-calibrated lossy compression; gain is in electrons per ADU. */
    public static void saveLossy(ImagePlus image, String filename, String dataset,
                                 double offsetAdu, double gainEPerAdu, double noiseStep) throws IOException {
        RadeltaHdf5Writer.write(image, filename, dataset,
            RadeltaHdf5Writer.Settings.lossy(offsetAdu, gainEPerAdu, noiseStep));
    }

    /**
     * Save with explicit chunk extents. Zero selects the default extent:
     * up to 4 planes in Z and 1024 pixels in Y and X, within 8 MiB per chunk.
     * T and C chunks always hold one index. Lossy parameters are ignored
     * when {@code lossy} is false.
     */
    public static void save(ImagePlus image, String filename, String dataset, boolean lossy,
                            double offsetAdu, double gainEPerAdu, double noiseStep,
                            int chunkZ, int chunkY, int chunkX) throws IOException {
        RadeltaHdf5Writer.Settings settings = lossy ?
            RadeltaHdf5Writer.Settings.lossy(offsetAdu, gainEPerAdu, noiseStep) :
            RadeltaHdf5Writer.Settings.lossless();
        settings.chunkZ = chunkZ;
        settings.chunkY = chunkY;
        settings.chunkX = chunkX;
        RadeltaHdf5Writer.write(image, filename, dataset, settings);
    }

    static String nativeResource(String operatingSystem, String architecture) {
        String os = operatingSystem.toLowerCase(Locale.ROOT), arch = architecture.toLowerCase(Locale.ROOT);
        boolean arm = arch.equals("aarch64") || arch.equals("arm64");
        boolean intel = arch.equals("amd64") || arch.equals("x86_64") || arch.equals("x64");
        if (os.startsWith("windows") && intel) return "/hdf5/windows-x86_64/H5Zradelta.dll";
        if (os.startsWith("linux") && intel) return "/hdf5/linux-x86_64/libH5Zradelta.so";
        if (os.startsWith("linux") && arm) return "/hdf5/linux-aarch64/libH5Zradelta.so";
        if ((os.startsWith("mac") || os.equals("darwin")) && intel) return "/hdf5/macos-x86_64/libH5Zradelta.dylib";
        if ((os.startsWith("mac") || os.equals("darwin")) && arm) return "/hdf5/macos-aarch64/libH5Zradelta.dylib";
        return null;
    }

    private RadeltaHdf5() {}
}
