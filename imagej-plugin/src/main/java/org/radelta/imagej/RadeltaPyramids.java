package org.radelta.imagej;

import com.sun.jna.Memory;
import com.sun.jna.ptr.IntByReference;

import java.io.IOException;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

/** Index into the TIFF metadata payload; compressed pixels are not copied. */
final class RadeltaPyramids {
    static final class Plane {
        final int width, height, offset, length, bitDepth;
        Plane(int width, int height, int offset, int length, int bitDepth) {
            this.width = width;
            this.height = height;
            this.offset = offset;
            this.length = length;
            this.bitDepth = bitDepth;
        }
    }

    static final class Level {
        final String path;
        final Plane[] planes; // Canonical TCZ order, unlike ImageJ's CZT order.
        final int width, height;
        Level(String path, Plane[] planes) {
            this.path = path;
            this.planes = planes;
            width = planes[0].width;
            height = planes[0].height;
        }
        String label() { return width + " x " + height + " (SubIFD " + path + ")"; }
    }

    final List<Level> levels = new ArrayList<Level>();
    int imageCount;

    static boolean isPyramid(byte[] data) {
        return data.length >= 8 && "RDTIFF02".equals(new String(data, 0, 8, StandardCharsets.US_ASCII));
    }

    static RadeltaPyramids parse(byte[] data, int expectedPages) throws IOException {
        RadeltaPyramids result = new RadeltaPyramids();
        if (!isPyramid(data)) return result;
        try {
            ByteBuffer in = ByteBuffer.wrap(data).order(ByteOrder.LITTLE_ENDIAN);
            in.position(9); // signature + layout-valid byte
            int pages = count(in, 8, "plane count");
            if (pages != expectedPages) throw new IOException("TIFF pyramid plane count mismatch");
            int[] mapping = new int[pages];
            boolean[] seen = new boolean[pages];
            for (int p = 0; p < pages; p++) {
                long canonical = in.getLong();
                if (canonical < 0 || canonical >= pages || seen[(int) canonical])
                    throw new IOException("Invalid TIFF pyramid plane mapping");
                mapping[p] = (int) canonical;
                seen[(int) canonical] = true;
            }
            long ifds = in.getLong();
            if (ifds != pages) throw new IOException("TIFF pyramid IFD count mismatch");
            for (int p = 0; p < pages; p++) RadeltaMetadata.readTags(in, null, 0);
            read(in, mapping, result);
            if (in.hasRemaining()) throw new IOException("Trailing TIFF pyramid metadata bytes");
            return result;
        } catch (java.nio.BufferUnderflowException | IllegalArgumentException e) {
            throw new IOException("Invalid TIFF pyramid metadata", e);
        }
    }

    // Also used by the calibration reader to validate and skip the extension.
    static void skip(ByteBuffer in, int pages) throws IOException {
        if (in.getLong() != pages) throw new IOException("TIFF pyramid parent count mismatch");
        for (int p = 0; p < pages; p++) {
            int children = count(in, 40, "image count");
            for (int i = 0; i < children; i++) image(in, "", p, 0, null, null);
        }
    }

    private static void read(ByteBuffer in, int[] mapping, RadeltaPyramids result) throws IOException {
        long parents = in.getLong();
        if (parents != mapping.length) throw new IOException("TIFF pyramid parent count mismatch");
        Map<String, Map<Integer, Plane>> groups = new LinkedHashMap<String, Map<Integer, Plane>>();
        for (int p = 0; p < mapping.length; p++) {
            int children = count(in, 40, "image count");
            for (int i = 0; i < children; i++)
                image(in, Integer.toString(i + 1), mapping[p], 0, groups, result);
        }
        for (Map.Entry<String, Map<Integer, Plane>> entry : groups.entrySet()) {
            if (entry.getValue().size() != mapping.length) continue;
            Plane[] planes = new Plane[mapping.length];
            for (Map.Entry<Integer, Plane> plane : entry.getValue().entrySet())
                planes[plane.getKey()] = plane.getValue();
            boolean complete = planes[0] != null;
            for (Plane plane : planes)
                if (plane == null || planes[0] == null || plane.width != planes[0].width
                        || plane.height != planes[0].height) complete = false;
            if (complete) {
                for (Plane plane : planes) if (plane.bitDepth != planes[0].bitDepth)
                    throw new IOException("Mixed pyramid sample depths are unsupported");
            }
            if (complete) result.levels.add(new Level(entry.getKey(), planes));
        }
    }

    private static void image(ByteBuffer in, String path, int parent, int depth,
                              Map<String, Map<Integer, Plane>> groups, RadeltaPyramids result) throws IOException {
        if (depth >= 16) throw new IOException("TIFF pyramid nesting exceeds 16");
        long width = in.getLong(), height = in.getLong();
        if (width <= 0 || height <= 0 || width > Integer.MAX_VALUE || height > Integer.MAX_VALUE
                || width * height > Integer.MAX_VALUE)
            throw new IOException("TIFF pyramid plane exceeds ImageJ array limits");
        Map<Integer, byte[]> tags = new LinkedHashMap<Integer, byte[]>();
        RadeltaMetadata.readTags(in, tags, depth);
        int bitDepth = RadeltaMetadata.savedBitDepth(tags);
        int length = count(in, 1, "codec length");
        if (length < 4 || in.get(in.position()) != 'R' || in.get(in.position() + 1) != 'D'
                || in.get(in.position() + 2) != 'L' || in.get(in.position() + 3) != '1')
            throw new IOException("TIFF pyramid pixels must use a lossless RDL1 stream");
        if (groups != null) {
            Map<Integer, Plane> planes = groups.get(path);
            if (planes == null) {
                planes = new LinkedHashMap<Integer, Plane>();
                groups.put(path, planes);
            }
            planes.put(parent, new Plane((int) width, (int) height, in.position(), length, bitDepth));
        }
        in.position(in.position() + length);
        if (result != null) result.imageCount++;
        int children = count(in, 40, "child count");
        for (int i = 0; i < children; i++)
            image(in, path + "." + (i + 1), parent, depth + 1, groups, result);
    }

    private static int count(ByteBuffer in, int minimumBytes, String name) throws IOException {
        long count = in.getLong();
        if (count < 0 || count > in.remaining() / minimumBytes)
            throw new IOException("Invalid TIFF pyramid " + name);
        return (int) count;
    }

    static short[] decode(byte[] metadata, Plane plane) throws IOException {
        short[] output = new short[Math.multiplyExact(plane.width, plane.height)];
        try (Memory input = new Memory(plane.length)) {
            input.write(0, metadata, plane.offset, plane.length);
            RadeltaNative.SizeTReference count = new RadeltaNative.SizeTReference();
            IntByReference x = new IntByReference(), y = new IntByReference(), z = new IntByReference();
            int rc = RadeltaNative.api().radelta_decompress_u16(input,
                    new RadeltaNative.SizeT(plane.length), output,
                    new RadeltaNative.SizeT(output.length), count, x, y, z);
            if (rc != RadeltaNative.OK || count.value() != output.length
                    || x.getValue() != plane.width || y.getValue() != plane.height || z.getValue() != 1)
                throw new IOException("Could not decode TIFF pyramid plane: corrupt pixels or dimensions");
        }
        return output;
    }
}
