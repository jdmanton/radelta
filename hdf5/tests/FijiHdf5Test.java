/* BSD-3-Clause; see ../../LICENSE. Headless tests against released Fiji jars.
 * Usage: FijiHdf5Test FIXTURE [--bundled [--write-dir DIR]]. With --write-dir,
 * the bundled plugin also writes datasets, checks them through Fiji and JHDF5,
 * and lists them in DIR/written.tsv for an independent native check. */
import java.io.File;
import java.io.IOException;
import java.io.PrintWriter;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.nio.file.Files;
import java.util.Arrays;
import ch.systemsx.cisd.hdf5.HDF5Factory;
import ch.systemsx.cisd.hdf5.IHDF5Reader;
import ch.systemsx.cisd.hdf5.IHDF5Writer;
import hdf.hdf5lib.H5;
import hdf.hdf5lib.HDF5Constants;
import ij.ImagePlus;
import ij.ImageStack;
import ij.measure.Calibration;
import ij.process.ByteProcessor;
import ij.process.FloatProcessor;
import ij.process.ShortProcessor;
import sc.fiji.hdf5.HDF5ImageJ;

public final class FijiHdf5Test {
    private static void check(boolean value, String message) {
        if (!value) throw new AssertionError(message);
    }

    public static void main(String[] args) throws Exception {
        boolean bundled = args.length > 1 && args[1].equals("--bundled");
        Object context = null;
        if (bundled) {
            check(System.getenv("HDF5_PLUGIN_PATH") == null, "Bundled test must not rely on HDF5_PLUGIN_PATH");
            Class<?> service = Class.forName("org.radelta.imagej.RadeltaHdf5Service");
            Class<?> contextClass = Class.forName("org.scijava.Context");
            context = contextClass.getConstructor().newInstance();
            check(contextClass.getMethod("getService", Class.class).invoke(context, service) != null,
                "SciJava did not discover the HDF5 startup service");
            System.out.println("ImageJ startup registered its bundled decoder without environment settings");
        }
        int[] version = new int[3];
        H5.H5get_libversion(version);
        System.out.println("Fiji/JHDF5 embedded HDF5: " + Arrays.toString(version));
        check(H5.H5Zfilter_avail(65024) > 0, "Radelta filter was not discovered");
        int config = H5.H5Zget_filter_info(65024);
        check((config & HDF5Constants.H5Z_FILTER_CONFIG_DECODE_ENABLED) != 0, "Decoder unavailable");
        // The embedded filter encodes when its writer supplies every parameter.
        check((config & HDF5Constants.H5Z_FILTER_CONFIG_ENCODE_ENABLED) != 0, "Embedded filter cannot encode");
        String[] names = {"lossless2d_le", "lossless3d_be", "lossless4d_le", "lossless5d_be",
            "lossy5d_le", "lossy3d_be"};
        String[] layouts = {"yx", "zyx", "czyx", "tczyx", "tczyx", "zyx"};
        try (IHDF5Reader reader = HDF5Factory.openForReading(args[0])) {
            for (int test = 0; test < names.length; ++test) {
                String path = "/" + names[test];
                short[] reference = reader.uint16().readMDArray("/expected" + path).getAsFlatArray();
                short[] actual = reader.uint16().readMDArray(path).getAsFlatArray();
                check(Arrays.equals(actual, reference), path + ": JHDF5 pixels differ");
                if (test < 4) {
                    for (int i = 0; i < actual.length; ++i)
                        check((actual[i] & 65535) == ((i * 17 + 32760) % 65536), path + ": unsigned pixels differ");
                }
                ImagePlus image = bundled ? (ImagePlus)Class.forName("org.radelta.imagej.RadeltaHdf5")
                    .getMethod("open", String.class, String.class, String.class).invoke(null, args[0], path, layouts[test]) :
                    HDF5ImageJ.hdf5read(args[0], path, layouts[test]);
                check(image != null, path + ": Fiji import failed");
                int channels = layouts[test].contains("c") ? 2 : 1;
                int frames = layouts[test].contains("t") ? 2 : 1;
                int levels = layouts[test].contains("z") ? 5 : 1;
                check(image.getWidth() == 17 && image.getHeight() == 13 && image.getBitDepth() == 16 &&
                    image.getNChannels() == channels && image.getNFrames() == frames &&
                    image.getNSlices() == levels, path + ": hyperstack dimensions differ");
                check(image.getCalibration().pixelWidth == 0.5 && image.getCalibration().pixelHeight == 0.75 &&
                    image.getCalibration().pixelDepth == 2.5, path + ": calibration differs");
                for (int t = 0; t < frames; ++t)
                    for (int c = 0; c < channels; ++c)
                        for (int z = 0; z < levels; ++z) {
                            short[] plane = (short[])image.getStack().getPixels(image.getStackIndex(c+1, z+1, t+1));
                            int offset = ((t * channels + c) * levels + z) * 13 * 17;
                            for (int i = 0; i < plane.length; ++i)
                                check(plane[i] == reference[offset+i], path + ": Fiji plane ordering/pixels differ");
                        }
                image.close();
                System.out.println(path + ": JHDF5 and Fiji hyperstack passed");
            }
            // Cross Z/Y/X chunk boundaries, with nonzero frame and channel.
            int[] size = {1, 1, 3, 4, 5};
            long[] start = {1, 1, 2, 5, 7};
            for (String path : new String[]{"/lossless5d_be", "/lossy5d_le"}) {
                short[] roi = reader.uint16().readMDArrayBlockWithOffset(path, size, start).getAsFlatArray();
                short[] reference = reader.uint16().readMDArray("/expected" + path).getAsFlatArray();
                int i = 0;
                for (int z = 2; z < 5; ++z)
                    for (int y = 5; y < 9; ++y)
                        for (int x = 7; x < 12; ++x)
                            check(roi[i++] == reference[(((1*2+1)*5+z)*13+y)*17+x], path + ": hyperslab differs");
            }
        }
        if (bundled && args.length > 3 && args[2].equals("--write-dir")) {
            writerTests(new File(args[3]));
            openerTests(new File(args[0]), new File(args[3]), context);
        }
        if (context != null) context.getClass().getMethod("dispose").invoke(context);
        System.out.println("Fiji HDF5: unsigned pixels, lossless/lossy, endian, edge chunks, axes, calibration and hyperslabs passed");
    }

    // File > Open and drag-and-drop through the bundled SciJava I/O hook.

    private static Object opener(String method, Class<?>[] types, Object... values) throws Exception {
        Method m = Class.forName("org.radelta.imagej.RadeltaHdf5Opener").getDeclaredMethod(method, types);
        m.setAccessible(true);
        try {
            return m.invoke(null, values);
        } catch (InvocationTargetException e) {
            if (e.getCause() instanceof Exception) throw (Exception) e.getCause();
            throw e;
        }
    }

    private static boolean supports(File file) throws Exception {
        return (Boolean) opener("supports", new Class<?>[] {String.class}, file.getPath());
    }

    /** Dataset paths and layouts the opener finds, e.g. "/a:zyx". */
    private static String catalogue(File file) throws Exception {
        StringBuilder out = new StringBuilder();
        for (Object dataset : (java.util.List<?>) opener("radeltaDatasets", new Class<?>[] {String.class}, file.getPath())) {
            java.lang.reflect.Field field = dataset.getClass().getDeclaredField("path");
            field.setAccessible(true);
            String path = (String) field.get(dataset);
            Method layout = dataset.getClass().getDeclaredMethod("layout");
            layout.setAccessible(true);
            out.append(out.length() == 0 ? "" : " ").append(path).append(':').append(layout.invoke(dataset));
        }
        return out.toString();
    }

    private static void checkOpensDirectly(File file, Case k) throws Exception {
        // Headless: a dialog would throw, so success shows the file opened directly.
        ImagePlus image = (ImagePlus) plugin("openFile", new Class<?>[] {String.class}, file.getPath());
        check(image != null && image.getNChannels() == k.nc && image.getNSlices() == k.nz && image.getNFrames() == k.nt,
            file.getName() + ": opened with the wrong shape");
        int[] want = expected(k);
        int plane = k.nx * k.ny;
        for (int t = 0; t < k.nt; ++t)
            for (int c = 0; c < k.nc; ++c)
                for (int z = 0; z < k.nz; ++z) {
                    short[] p = (short[]) image.getStack().getPixels(image.getStackIndex(c + 1, z + 1, t + 1));
                    for (int i = 0; i < plane; ++i)
                        check((p[i] & 65535) == want[((t * k.nc + c) * k.nz + z) * plane + i], file.getName() + ": pixels differ");
                }
        image.close();
    }

    private static void openerTests(File fixture, File dir, Object context) throws Exception {
        // The fixture holds six Radelta datasets without axes attributes, plus
        // uncompressed references that must not be offered.
        check(supports(fixture), "Fixture with Radelta datasets was not claimed");
        check(catalogue(fixture).equals("/lossless2d_le:yx /lossless3d_be:zyx /lossless4d_le:czyx " +
            "/lossless5d_be:tczyx /lossy3d_be:zyx /lossy5d_le:tczyx"), "Fixture catalogue: " + catalogue(fixture));

        // Files written by the plugin record their layout and open without a dialog.
        Case edges = new Case("written_3d_edges", 17, 13, 5, 1, 1, 16, false, 2, 7, 9, "um", 0.5, "/images");
        Case lossy = new Case("written_5d_lossy", 17, 13, 5, 2, 2, 16, true, 3, 7, 9, "um", 0.5, "/images");
        Case nested = new Case("written_4d_8bit_nm", 11, 9, 3, 2, 1, 8, false, 2, 5, 4, "nm", 120.0, "/groups/inside/images");
        for (Case k : new Case[] {edges, lossy, nested}) {
            File file = new File(dir, k.name + ".h5");
            check(supports(file), file.getName() + ": not claimed");
            check(catalogue(file).equals(k.dataset + ":" + k.layout()), file.getName() + ": catalogue " + catalogue(file));
            checkOpensDirectly(file, k);
        }

        // h5py and the radelta-hdf5 tools write axes as a variable-length string.
        File variable = new File(dir, "variable_axes.h5");
        Files.copy(new File(dir, "written_5d_lossy.h5").toPath(), variable.toPath());
        try (IHDF5Writer writer = HDF5Factory.open(variable)) {
            writer.object().deleteAttribute("/images", "axes");
            writer.string().setAttrVL("/images", "axes", "TCZYX");
        }
        check(catalogue(variable).equals("/images:tczyx"), "Variable-length axes: " + catalogue(variable));
        checkOpensDirectly(variable, lossy);

        // Other files are left to their own readers.
        File plain = new File(dir, "plain.h5");
        try (IHDF5Writer writer = HDF5Factory.open(plain)) {
            writer.uint16().writeArray("/images", new short[] {1, 2, 3, 4});
        }
        File notHdf5 = new File(dir, "not_hdf5.h5");
        Files.write(notHdf5.toPath(), "not an HDF5 file".getBytes("UTF-8"));
        File otherExtension = new File(dir, "radelta.dat");
        Files.copy(new File(dir, "written_2d.h5").toPath(), otherExtension.toPath());
        for (File file : new File[] {plain, notHdf5, otherExtension, new File(dir, "missing.h5")})
            check(!supports(file), file.getName() + ": claimed by the Radelta HDF5 hook");

        // Both routes Fiji uses for File > Open and drag-and-drop must reach the
        // hooks: the legacy bridge's eager lookup (the default) and, with the
        // "SciJava I/O" option, IOService.getOpener on a resolved location.
        Method service = context.getClass().getMethod("getService", Class.class);
        Object io = service.invoke(context, Class.forName("org.scijava.io.IOService"));
        Object plugins = service.invoke(context, Class.forName("org.scijava.plugin.PluginService"));
        String radelta = new File(dir, "written_2d.h5").getPath(), rdlt = new File(dir, "image.rdlt").getPath();
        String[][] routes = {
            {radelta, "org.radelta.imagej.RadeltaHdf5IOPlugin"},
            {rdlt, "org.radelta.imagej.RadeltaIOPlugin"},
            {plain.getPath(), null},
        };
        Method getOpener = io.getClass().getMethod("getOpener", String.class);
        for (String[] route : routes) {
            Object eager = null;
            for (Object info : (java.util.List<?>) io.getClass().getMethod("getPlugins").invoke(io)) {
                if (!(Boolean) info.getClass().getMethod("is", String.class).invoke(info, "eager")) continue;
                Object instance = plugins.getClass().getMethod("createInstance", Class.forName("org.scijava.plugin.PluginInfo"))
                    .invoke(plugins, info);
                if ((Boolean) instance.getClass().getMethod("supportsOpen", String.class).invoke(instance, route[0])) {
                    eager = instance;
                    break;
                }
            }
            Object resolved = getOpener.invoke(io, route[0]);
            for (Object chosen : new Object[] {eager, resolved}) {
                String name = chosen == null ? null : chosen.getClass().getName();
                if (route[1] == null) {
                    check(name == null || !name.startsWith("org.radelta."), "A Radelta hook claimed " + route[0]);
                } else {
                    check(route[1].equals(name), (chosen == eager ? "Eager lookup" : "getOpener") + " chose " + name +
                        " for " + new File(route[0]).getName());
                }
            }
        }
        System.out.println("Fiji HDF5 opening: detection, catalogue, recorded layouts, variable-length axes and SciJava hook passed");
    }

    // Writing through the bundled plugin.

    private static final class Case {
        final String name, dataset, unit;
        final int nx, ny, nz, nc, nt, bits, chunkZ, chunkY, chunkX;
        final boolean lossy;
        final double spacing;

        Case(String name, int nx, int ny, int nz, int nc, int nt, int bits, boolean lossy,
             int chunkZ, int chunkY, int chunkX, String unit, double spacing, String dataset) {
            this.name = name; this.nx = nx; this.ny = ny; this.nz = nz; this.nc = nc; this.nt = nt;
            this.bits = bits; this.lossy = lossy; this.chunkZ = chunkZ; this.chunkY = chunkY; this.chunkX = chunkX;
            this.unit = unit; this.spacing = spacing; this.dataset = dataset;
        }

        int rank() {
            return nt > 1 ? 5 : nc > 1 ? 4 : nz > 1 ? 3 : 2;
        }

        String layout() {
            return "tczyx".substring(5 - rank());
        }

        long[] shape() {
            long[] full = {nt, nc, nz, ny, nx};
            return Arrays.copyOfRange(full, 5 - rank(), 5);
        }

        /** Canonical T,C,Z,Y,X source pixels; lossy cases stay in a camera-like range. */
        int pixel(int t, int c, int z, int y, int x) {
            long i = (((long) t * nc + c) * nz + z) * ny * nx + (long) y * nx + x;
            if (bits == 8) return (int) ((i * 7 + 3) % 256);
            if (lossy) return 100 + (int) ((x * 13 + y * 7 + z * 29 + c * 101 + t * 211 + i % 5) % 4000);
            if (i == 0) return 0;
            if (i == 1) return 65535;
            return (int) ((i * 17 + 32760) % 65536);
        }
    }

    private static final double OFFSET = 100.0, GAIN = 0.46, NOISE = 2.0;

    // The codec's calibrated quantiser and reconstruction, for exact checks.
    private static int lossy(int v) {
        double electrons = Math.max((v - OFFSET) * GAIN, 0.0);
        long q = Math.round(2.0 * Math.sqrt(electrons) / NOISE);
        double z = q * NOISE;
        double adu = OFFSET + 0.25 * z * z / GAIN;
        return (int) Math.max(0, Math.min(65535, Math.round(adu)));
    }

    private static ImagePlus image(Case k) {
        ImageStack stack = new ImageStack(k.nx, k.ny);
        // ImageJ hyperstacks interleave C fastest, then Z, then T.
        for (int t = 0; t < k.nt; ++t)
            for (int z = 0; z < k.nz; ++z)
                for (int c = 0; c < k.nc; ++c) {
                    if (k.bits == 8) {
                        byte[] p = new byte[k.nx * k.ny];
                        for (int y = 0; y < k.ny; ++y)
                            for (int x = 0; x < k.nx; ++x) p[y * k.nx + x] = (byte) k.pixel(t, c, z, y, x);
                        stack.addSlice(new ByteProcessor(k.nx, k.ny, p, null));
                    } else {
                        short[] p = new short[k.nx * k.ny];
                        for (int y = 0; y < k.ny; ++y)
                            for (int x = 0; x < k.nx; ++x) p[y * k.nx + x] = (short) k.pixel(t, c, z, y, x);
                        stack.addSlice(new ShortProcessor(k.nx, k.ny, p, null));
                    }
                }
        ImagePlus imp = new ImagePlus(k.name, stack);
        imp.setDimensions(k.nc, k.nz, k.nt);
        if (k.nc * k.nz * k.nt > 1) imp.setOpenAsHyperStack(true);
        Calibration cal = imp.getCalibration();
        if (k.unit != null) {
            cal.setUnit(k.unit);
            cal.pixelWidth = k.spacing;
            cal.pixelHeight = 2 * k.spacing;
            cal.pixelDepth = 3 * k.spacing;
        }
        return imp;
    }

    private static int[] expected(Case k) {
        int[] out = new int[k.nt * k.nc * k.nz * k.ny * k.nx];
        int i = 0;
        for (int t = 0; t < k.nt; ++t)
            for (int c = 0; c < k.nc; ++c)
                for (int z = 0; z < k.nz; ++z)
                    for (int y = 0; y < k.ny; ++y)
                        for (int x = 0; x < k.nx; ++x) {
                            int v = k.pixel(t, c, z, y, x);
                            out[i++] = k.lossy ? lossy(v) : v;
                        }
        return out;
    }

    private static Object plugin(String method, Class<?>[] types, Object... values) throws Exception {
        Method m = Class.forName("org.radelta.imagej.RadeltaHdf5").getMethod(method, types);
        try {
            return m.invoke(null, values);
        } catch (InvocationTargetException e) {
            if (e.getCause() instanceof Exception) throw (Exception) e.getCause();
            throw e;
        }
    }

    private static void save(ImagePlus imp, File file, Case k) throws Exception {
        plugin("save", new Class<?>[] {ImagePlus.class, String.class, String.class, boolean.class,
                double.class, double.class, double.class, int.class, int.class, int.class},
            imp, file.getPath(), k.dataset, k.lossy, OFFSET, GAIN, NOISE, k.chunkZ, k.chunkY, k.chunkX);
    }

    private static ImagePlus open(File file, Case k) throws Exception {
        return (ImagePlus) plugin("open", new Class<?>[] {String.class, String.class, String.class},
            file.getPath(), k.dataset, k.layout());
    }

    /** Expected HDF5 chunks: requested extents, else up to 4 Z planes and the full (small) XY plane. */
    private static long[] expectedChunks(Case k) {
        long[] shape = k.shape();
        int rank = shape.length;
        long[] chunks = new long[rank];
        Arrays.fill(chunks, 1);
        chunks[rank - 1] = k.chunkX > 0 ? k.chunkX : shape[rank - 1];
        chunks[rank - 2] = k.chunkY > 0 ? k.chunkY : shape[rank - 2];
        if (rank >= 3) chunks[rank - 3] = k.chunkZ > 0 ? k.chunkZ : Math.min(shape[rank - 3], 4);
        return chunks;
    }

    /**
     * The stored chunk shape and all 17 filter parameters must match what the
     * SDK filter's set_local derives. Wrong geometry still round-trips, so
     * pixel checks alone cannot catch it; it would only hurt compression.
     */
    private static void checkFilterParameters(File file, Case k) {
        long[] chunks = expectedChunks(k);
        int rank = chunks.length;
        int[] want = new int[17];
        want[0] = 1; want[1] = k.lossy ? 1 : 0; want[2] = 4; want[3] = 2; want[4] = 10;
        double[] values = {k.lossy ? OFFSET : 0.0, k.lossy ? GAIN : 1.0, k.lossy ? NOISE : 2.0};
        for (int i = 0; i < 3; ++i) {
            long bits = Double.doubleToRawLongBits(values[i]);
            want[5 + 2 * i] = (int) bits;
            want[6 + 2 * i] = (int) (bits >>> 32);
        }
        for (int i = 0; i < 5; ++i) want[11 + i] = i < rank ? (int) chunks[rank - 1 - i] : 1;
        want[16] = 0;
        long fileId = H5.H5Fopen(file.getPath(), HDF5Constants.H5F_ACC_RDONLY, HDF5Constants.H5P_DEFAULT);
        try {
            long dataset = H5.H5Dopen(fileId, k.dataset, HDF5Constants.H5P_DEFAULT);
            try {
                long dcpl = H5.H5Dget_create_plist(dataset);
                try {
                    long[] stored = new long[rank];
                    check(H5.H5Pget_chunk(dcpl, rank, stored) == rank && Arrays.equals(stored, chunks),
                        k.name + ": chunk shape " + Arrays.toString(stored) + ", expected " + Arrays.toString(chunks));
                    check(H5.H5Pget_nfilters(dcpl) == 1, k.name + ": Radelta must be the only filter");
                    int[] flags = new int[1], config = new int[1], params = new int[17];
                    long[] count = {params.length};
                    String[] name = new String[1];
                    check(H5.H5Pget_filter(dcpl, 0, flags, count, params, 256, name, config) == 65024,
                        k.name + ": first filter is not Radelta");
                    check(count[0] == 17 && Arrays.equals(params, want),
                        k.name + ": filter parameters " + Arrays.toString(params) + ", expected " + Arrays.toString(want));
                    check((flags[0] & HDF5Constants.H5Z_FLAG_OPTIONAL) == 0, k.name + ": filter must be mandatory");
                } finally {
                    H5.H5Pclose(dcpl);
                }
            } finally {
                H5.H5Dclose(dataset);
            }
        } finally {
            H5.H5Fclose(fileId);
        }
    }

    /** Check one written file through JHDF5 and Fiji's importer, then list it for the native check. */
    private static void verify(File file, Case k, PrintWriter manifest, File dir) throws Exception {
        int[] want = expected(k);
        checkFilterParameters(file, k);
        try (IHDF5Reader reader = HDF5Factory.openForReading(file)) {
            short[] flat = reader.uint16().readMDArray(k.dataset).getAsFlatArray();
            check(flat.length == want.length, k.name + ": JHDF5 element count differs");
            for (int i = 0; i < flat.length; ++i)
                check((flat[i] & 65535) == want[i], k.name + ": JHDF5 pixel " + i + " differs");
            int[] dims = reader.object().getDimensions(k.dataset) == null ? null :
                Arrays.stream(reader.object().getDimensions(k.dataset)).mapToInt(v -> (int) v).toArray();
            int[] shape = Arrays.stream(k.shape()).mapToInt(v -> (int) v).toArray();
            check(Arrays.equals(dims, shape), k.name + ": dataset shape " + Arrays.toString(dims));
            check(reader.string().getAttr(k.dataset, "axes").equals(k.layout().toUpperCase()), k.name + ": axes attribute");
            check(!reader.string().getAttr(k.dataset, "radelta_version").isEmpty(), k.name + ": version attribute");
            check(reader.uint8().getAttr(k.dataset, "radelta_lossy") == (k.lossy ? 1 : 0), k.name + ": lossy attribute");
            if (k.lossy) {
                check(reader.float64().getAttr(k.dataset, "offset_adu") == OFFSET &&
                    reader.float64().getAttr(k.dataset, "gain_e_per_adu") == GAIN &&
                    reader.float64().getAttr(k.dataset, "noise_step") == NOISE, k.name + ": lossy calibration attributes");
            }
            boolean calibrated = reader.object().hasAttribute(k.dataset, "element_size_um");
            check(calibrated == (k.unit != null), k.name + ": element_size_um presence");
            if (calibrated) {
                double um = k.unit.equals("nm") ? 1e-3 : 1.0;
                float[] spacing = reader.float32().getArrayAttr(k.dataset, "element_size_um");
                float[] wanted = {(float) (3 * k.spacing * um), (float) (2 * k.spacing * um), (float) (k.spacing * um)};
                check(Arrays.equals(spacing, wanted), k.name + ": element_size_um " + Arrays.toString(spacing));
            }
        }
        ImagePlus back = open(file, k);
        check(back != null && back.getWidth() == k.nx && back.getHeight() == k.ny && back.getBitDepth() == 16 &&
            back.getNChannels() == k.nc && back.getNSlices() == k.nz && back.getNFrames() == k.nt,
            k.name + ": Fiji hyperstack dimensions differ");
        int plane = k.nx * k.ny;
        for (int t = 0; t < k.nt; ++t)
            for (int c = 0; c < k.nc; ++c)
                for (int z = 0; z < k.nz; ++z) {
                    short[] p = (short[]) back.getStack().getPixels(back.getStackIndex(c + 1, z + 1, t + 1));
                    int offset = ((t * k.nc + c) * k.nz + z) * plane;
                    for (int i = 0; i < plane; ++i)
                        check((p[i] & 65535) == want[offset + i], k.name + ": Fiji pixel differs");
                }
        if (k.unit != null) {
            double um = k.unit.equals("nm") ? 1e-3 : 1.0;
            Calibration cal = back.getCalibration();
            check(Math.abs(cal.pixelWidth - k.spacing * um) < 1e-6 && Math.abs(cal.pixelHeight - 2 * k.spacing * um) < 1e-6 &&
                Math.abs(cal.pixelDepth - 3 * k.spacing * um) < 1e-6, k.name + ": Fiji calibration differs");
        }
        back.close();
        File raw = new File(dir, k.name + ".u16le");
        byte[] bytes = new byte[2 * want.length];
        for (int i = 0; i < want.length; ++i) {
            bytes[2 * i] = (byte) want[i];
            bytes[2 * i + 1] = (byte) (want[i] >>> 8);
        }
        Files.write(raw.toPath(), bytes);
        manifest.println(file.getName() + "\t" + k.dataset + "\t" + raw.getName());
        System.out.println(k.name + ": written by the plugin; JHDF5 and Fiji import passed");
    }

    private static void checkNoTemporaryFiles(File dir) {
        String[] leftovers = dir.list((d, name) -> name.endsWith(".tmp"));
        check(leftovers != null && leftovers.length == 0, "Temporary files were left behind: " + Arrays.toString(leftovers));
    }

    private static void writerTests(File dir) throws Exception {
        check(dir.isDirectory() || dir.mkdirs(), "Cannot create " + dir);
        Case[] cases = {
            new Case("written_2d", 17, 13, 1, 1, 1, 16, false, 0, 0, 0, "um", 0.25, "/images"),
            // Chunks that leave partial edge chunks in Z, Y and X.
            new Case("written_3d_edges", 17, 13, 5, 1, 1, 16, false, 2, 7, 9, "um", 0.5, "/images"),
            new Case("written_4d_8bit_nm", 11, 9, 3, 2, 1, 8, false, 2, 5, 4, "nm", 120.0, "/groups/inside/images"),
            new Case("written_5d_uncalibrated", 10, 6, 2, 1, 3, 16, false, 0, 0, 0, null, 0, "/images"),
            new Case("written_5d_lossy", 17, 13, 5, 2, 2, 16, true, 3, 7, 9, "um", 0.5, "/images"),
            new Case("written_3d_lossy_large_chunk", 33, 21, 6, 1, 1, 16, true, 0, 0, 0, "um", 1.0, "/images"),
        };
        try (PrintWriter manifest = new PrintWriter(new File(dir, "written.tsv"), "UTF-8")) {
            for (Case k : cases) {
                File file = new File(dir, k.name + ".h5");
                save(image(k), file, k);
                verify(file, k, manifest, dir);
            }
            // Replacing an existing file must leave exactly the new dataset.
            File replaced = new File(dir, "written_replaced.h5");
            save(image(cases[1]), replaced, cases[1]);
            Case replacement = new Case("written_replaced", 9, 8, 1, 1, 1, 16, false, 0, 0, 0, null, 0, "/images");
            save(image(replacement), replaced, replacement);
            try (IHDF5Reader reader = HDF5Factory.openForReading(replaced)) {
                check(reader.object().getGroupMemberPaths("/").size() == 1, "Replaced file kept old objects");
            }
            verify(replaced, replacement, manifest, dir);
        }
        // A plane that cannot be read must abort the save, keeping any existing file.
        Case failing = new Case("unreadable", 9, 8, 4, 1, 1, 16, false, 2, 0, 0, null, 0, "/images");
        ImagePlus good = image(failing);
        ImageStack broken = new ImageStack(failing.nx, failing.ny) {
            @Override
            public Object getPixels(int n) {
                if (n == 3) throw new java.io.UncheckedIOException(new IOException("plane 3 checksum mismatch"));
                return good.getStack().getPixels(n);
            }

            @Override
            public ij.process.ImageProcessor getProcessor(int n) {
                getPixels(n);
                return good.getStack().getProcessor(n);
            }

            @Override
            public int getSize() {
                return good.getStackSize();
            }
        };
        for (int i = 1; i <= good.getStackSize(); ++i) broken.addSlice("", good.getStack().getPixels(i));
        ImagePlus unreadable = new ImagePlus("unreadable", broken);
        for (boolean existing : new boolean[] {false, true}) {
            File target = new File(dir, "unreadable.h5");
            byte[] previous = "previous complete file".getBytes("UTF-8");
            if (existing) Files.write(target.toPath(), previous);
            try {
                save(unreadable, target, failing);
                throw new AssertionError("An unreadable plane was exported");
            } catch (IOException expected) {
                check(expected.getMessage().contains("Z=3") && expected.getCause() != null &&
                    expected.getCause().getMessage().contains("checksum mismatch"), "Unexpected failure: " + expected);
            }
            if (existing) {
                check(Arrays.equals(previous, Files.readAllBytes(target.toPath())), "A failed save changed the existing file");
                check(target.delete(), "Cannot remove " + target);
            } else {
                check(!target.exists(), "A failed save created its target file");
            }
            checkNoTemporaryFiles(dir);
        }

        // A failed save must not create or damage the target file.
        File rejected = new File(dir, "rejected.h5");
        ImagePlus floating = new ImagePlus("float", new FloatProcessor(4, 4));
        try {
            save(floating, rejected, new Case("rejected", 4, 4, 1, 1, 1, 16, false, 0, 0, 0, null, 0, "/images"));
            throw new AssertionError("32-bit images must be rejected");
        } catch (IOException expected) {
            check(!rejected.exists(), "A rejected save created its target file");
        }
        checkNoTemporaryFiles(dir);
        System.out.println("Fiji HDF5 writing: lossless/lossy, ranks 2-5, 8/16-bit, edge chunks, calibration, groups, replacement and failed reads passed");
    }
}
