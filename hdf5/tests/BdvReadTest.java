/* Headless integration with Fiji's released BDV XML/HDF5 reader. */
import bdv.img.hdf5.Hdf5ImageLoader;
import bdv.spimdata.SpimDataMinimal;
import bdv.spimdata.XmlIoSpimDataMinimal;
import hdf.hdf5lib.H5;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;
import net.imglib2.RandomAccessibleInterval;
import net.imglib2.type.numeric.integer.UnsignedShortType;
import net.imglib2.view.Views;

public class BdvReadTest {
    private static void check(boolean condition, String message) {
        if (!condition) throw new AssertionError(message);
    }

    public static void main(String[] args) throws Exception {
        // Register the runtime-independent embedded filter in JHDF5's runtime,
        // exactly as Radelta's Fiji startup service does.
        H5.H5PLprepend(args[2]);
        check(H5.H5Zfilter_avail(65024) > 0, "Radelta filter unavailable to BDV");
        SpimDataMinimal data = new XmlIoSpimDataMinimal().load(args[0]);
        Hdf5ImageLoader loader = (Hdf5ImageLoader) data.getSequenceDescription().getImgLoader();
        Path directory = Paths.get(args[1]);
        try {
            check(data.getSequenceDescription().getViewSetupsOrdered().size() == 2, "Wrong setup count");
            check(data.getSequenceDescription().getTimePoints().size() == 2, "Wrong timepoint count");
            double[] voxel = new double[3];
            data.getSequenceDescription().getViewSetups().get(0).getVoxelSize().dimensions(voxel);
            check(voxel[0] == .25 && voxel[1] == .5 && voxel[2] == 2, "Wrong XYZ calibration");
            double[][] factors = {{1, 1, 1}, {2, 2, 1}, {4, 4, 2}};
            int images = 0;
            for (String line : Files.readAllLines(directory.resolve("expected.tsv"))) {
                String[] fields = line.split("\t");
                int t = Integer.parseInt(fields[0]);
                int c = Integer.parseInt(fields[1]);
                int level = Integer.parseInt(fields[2]);
                Hdf5ImageLoader.SetupImgLoader<?, ?> setup = loader.getSetupImgLoader(c);
                check(setup.numMipmapLevels() == 3, "Wrong mipmap count");
                for (int d = 0; d < 3; ++d) {
                    check(setup.getMipmapResolutions()[level][d] == factors[level][d], "Wrong mipmap factors");
                    check(setup.getMipmapTransforms()[level].get(d, 3) == .5*(factors[level][d]-1),
                          "Wrong voxel-centre transform");
                }
                RandomAccessibleInterval<?> image = setup.getImage(t, level);
                for (int d = 0; d < 3; ++d)
                    check(image.dimension(d) == Long.parseLong(fields[3+d]), "Wrong XYZ dimensions");
                ByteBuffer reference = ByteBuffer.wrap(Files.readAllBytes(directory.resolve(fields[6])))
                                                .order(ByteOrder.LITTLE_ENDIAN);
                int i = 0;
                for (Object value : Views.flatIterable(image)) {
                    check(value instanceof UnsignedShortType, "BDV did not expose unsigned uint16 pixels");
                    UnsignedShortType pixel = (UnsignedShortType) value;
                    int expected = reference.getShort() & 0xffff;
                    check(pixel.get() == expected, "Pixels differ: t="+t+" c="+c+" level="+level+" index="+i);
                    ++i;
                }
                check(!reference.hasRemaining(), "Reference contains extra pixels");
                ++images;
            }
            check(images == 12, "Did not check every image and level");
            System.out.println("BDV XML loading, calibration, mipmap transforms and all 12 image volumes passed");
        } finally {
            loader.close();
        }
    }
}
