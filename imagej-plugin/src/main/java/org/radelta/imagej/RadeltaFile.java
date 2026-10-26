package org.radelta.imagej;

import com.sun.jna.Pointer;
import com.sun.jna.Memory;
import com.sun.jna.ptr.PointerByReference;

import java.io.Closeable;
import java.io.IOException;
import java.math.BigInteger;
import java.nio.file.Path;
import java.nio.file.Paths;

final class RadeltaFile implements Closeable {
    private Pointer handle;
    private Memory batchBuffer;
    final Path sourcePath;
    int width, height;
    final int fullWidth, fullHeight, slices, channels, frames;
    private byte[] metadata, tiffMetadata;
    private RadeltaPyramids pyramids;
    private int resolution;
    final int format, flags;
    final boolean lossy, streaming;

    private RadeltaFile(Pointer handle, RadeltaNative.FileInfo info, String path) throws IOException {
        this.handle = handle;
        this.sourcePath = Paths.get(path).toRealPath();
        this.width = positive(info.nx, "X");
        this.height = positive(info.ny, "Y");
        this.fullWidth = width;
        this.fullHeight = height;
        this.slices = positive(info.nz, "Z");
        this.channels = positive(info.nc, "C");
        this.frames = positive(info.nt, "T");
        this.format = info.format;
        this.flags = info.flags;
        this.lossy = (flags & RadeltaNative.FLAG_LOSSY) != 0;
        this.streaming = (flags & RadeltaNative.FLAG_STREAMING) != 0;
        long plane = (long) width * (long) height;
        if (plane > Integer.MAX_VALUE) throw new IOException("A single Radelta plane exceeds Java array limits");
        BigInteger planes = BigInteger.valueOf(slices).multiply(BigInteger.valueOf(channels))
                .multiply(BigInteger.valueOf(frames));
        if (planes.compareTo(BigInteger.valueOf(Integer.MAX_VALUE)) > 0) throw new IOException("Radelta stack contains too many planes for ImageJ 1.x indexing");
    }

    static RadeltaFile open(String path) throws IOException {
        RadeltaNative.Api api = RadeltaNative.api();
        PointerByReference ref = new PointerByReference();
        int rc = api.radelta_file_open(path, ref);
        if (rc != RadeltaNative.OK || ref.getValue() == null) {
            throw new IOException("Could not open Radelta file: " + RadeltaNative.lastError());
        }
        Pointer h = ref.getValue();
        RadeltaNative.FileInfo info = new RadeltaNative.FileInfo();
        rc = api.radelta_file_get_info(h, info);
        info.read();
        if (rc != RadeltaNative.OK) {
            api.radelta_file_close(h);
            throw new IOException("Could not read Radelta metadata: " + RadeltaNative.lastError());
        }
        try {
            return new RadeltaFile(h, info, path);
        } catch (IOException e) {
            api.radelta_file_close(h);
            throw e;
        }
    }

    synchronized byte[] readMetadata() throws IOException {
        if (handle == null) throw new IOException("Radelta file is closed");
        if (metadata != null) return metadata;
        RadeltaNative.SizeTReference size = new RadeltaNative.SizeTReference();
        int rc = RadeltaNative.api().radelta_file_get_metadata(handle, null, new RadeltaNative.SizeT(), size);
        if (rc != RadeltaNative.OK && rc != 1) throw new IOException("Could not read Radelta metadata");
        long length = size.value();
        if (length < 0 || length > Integer.MAX_VALUE) throw new IOException("Metadata exceeds Java array limits");
        if (length == 0) return new byte[0];
        byte[] data = new byte[(int)length];
        rc = RadeltaNative.api().radelta_file_get_metadata(handle, data, new RadeltaNative.SizeT(length), size);
        if (rc != RadeltaNative.OK || size.value() != length) throw new IOException("Could not read Radelta metadata");
        metadata = data;
        return data;
    }

    synchronized RadeltaPyramids pyramids() throws IOException {
        if (pyramids == null) {
            tiffMetadata = RadeltaMetadata.sourceTiff(readMetadata());
            pyramids = RadeltaPyramids.parse(tiffMetadata, planeCount());
        }
        return pyramids;
    }

    synchronized void selectResolution(int index) throws IOException {
        if (handle == null) throw new IOException("Radelta file is closed");
        if (index < 0 || index > pyramids().levels.size())
            throw new IOException("No stored Radelta resolution " + index);
        resolution = index;
        width = index == 0 ? fullWidth : pyramids.levels.get(index - 1).width;
        height = index == 0 ? fullHeight : pyramids.levels.get(index - 1).height;
    }

    int resolution() { return resolution; }

    synchronized short[] readPlane(int t, int c, int z) throws IOException {
        if (handle == null) throw new IOException("Radelta file is closed");
        if (t < 0 || t >= frames || c < 0 || c >= channels || z < 0 || z >= slices)
            throw new IOException("Radelta plane coordinates out of range");
        if (resolution != 0)
            return RadeltaPyramids.decode(tiffMetadata,
                    pyramids.levels.get(resolution - 1).planes[(t * channels + c) * slices + z]);
        short[] pixels = new short[Math.multiplyExact(width, height)];
        int rc = RadeltaNative.api().radelta_file_read_plane_u16(
                handle, t, c, z, pixels, pixels.length);
        if (rc != RadeltaNative.OK) {
            throw new IOException("Radelta decode failed: " + RadeltaNative.lastError());
        }
        return pixels;
    }

    /** Read TCZ triples into independently owned Java plane arrays. */
    synchronized short[][] readPlanes(int[] tcz) throws IOException {
        if (handle == null) throw new IOException("Radelta file is closed");
        if (tcz.length % 3 != 0) throw new IllegalArgumentException("Expected TCZ triples");
        final int count = tcz.length / 3;
        final int plane = Math.multiplyExact(width, height);
        final short[][] result = new short[count][];
        if (count == 0) return result;
        if (count == 1 || resolution != 0) {
            for (int i = 0; i < count; i++)
                result[i] = readPlane(tcz[3 * i], tcz[3 * i + 1], tcz[3 * i + 2]);
            return result;
        }
        final long voxels = Math.multiplyExact((long) plane, count);
        final long bytes = Math.multiplyExact(voxels, 2L);
        if (batchBuffer == null || batchBuffer.size() < bytes) {
            if (batchBuffer != null) batchBuffer.close();
            batchBuffer = null;
            batchBuffer = new Memory(bytes);
        }
        int rc = RadeltaNative.api().radelta_file_read_planes_u16(handle, count, tcz, batchBuffer, voxels);
        if (rc != RadeltaNative.OK) {
            throw new IOException("Radelta decode failed: " + RadeltaNative.lastError());
        }
        for (int i = 0; i < count; ++i) {
            result[i] = new short[plane];
            batchBuffer.read((long) i * plane * 2L, result[i], 0, plane);
        }
        return result;
    }

    /** Bound the extra native transfer buffer to 64 MiB and tiny-plane batches
     * to 128 entries. A single larger plane uses the existing one-plane path. */
    int loadBatchSize() {
        return (int) Math.max(1L, Math.min(128L, (64L << 20) / ((long) width * height * 2L)));
    }

    int planeCount() {
        return Math.multiplyExact(Math.multiplyExact(channels, slices), frames);
    }

    BigInteger pixelBytesBig() {
        return BigInteger.valueOf(width)
                .multiply(BigInteger.valueOf(height))
                .multiply(BigInteger.valueOf(slices))
                .multiply(BigInteger.valueOf(channels))
                .multiply(BigInteger.valueOf(frames))
                .multiply(BigInteger.valueOf(2L));
    }

    @Override
    public synchronized void close() {
        if (batchBuffer != null) {
            batchBuffer.close();
            batchBuffer = null;
        }
        if (handle != null) {
            RadeltaNative.api().radelta_file_close(handle);
            handle = null;
        }
    }

    private static int positive(int v, String axis) throws IOException {
        long u = Integer.toUnsignedLong(v);
        if (u == 0 || u > Integer.MAX_VALUE) throw new IOException("Unsupported " + axis + " dimension: " + u);
        return (int) u;
    }
}
