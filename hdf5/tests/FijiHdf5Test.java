/* BSD-3-Clause; see ../../LICENSE. Headless tests against released Fiji jars. */
import java.util.Arrays;
import ch.systemsx.cisd.hdf5.HDF5Factory;
import ch.systemsx.cisd.hdf5.IHDF5Reader;
import hdf.hdf5lib.H5;
import hdf.hdf5lib.HDF5Constants;
import ij.ImagePlus;
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
        check((config & HDF5Constants.H5Z_FILTER_CONFIG_ENCODE_ENABLED) == 0, "Expected read-only filter");
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
        if (context != null) context.getClass().getMethod("dispose").invoke(context);
        System.out.println("Fiji HDF5: unsigned pixels, lossless/lossy, endian, edge chunks, axes, calibration and hyperslabs passed");
    }
}
