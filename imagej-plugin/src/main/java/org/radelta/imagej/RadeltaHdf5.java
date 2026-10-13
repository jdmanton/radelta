package org.radelta.imagej;

import ij.ImagePlus;
import java.io.IOException;
import java.io.InputStream;
import java.lang.reflect.InvocationTargetException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.util.Locale;

/** Register our bundled decoder in the reader's own HDF5 runtime. */
public final class RadeltaHdf5 {
    private static Path configuredDirectory;

    private static Class<?> readerClass(String name) throws ClassNotFoundException {
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
            if ((flags & 1) != 0) throw new IOException("A full Radelta HDF5 filter is already registered. Clear the older HDF5_PLUGIN_PATH setting and restart Fiji.");
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
