package org.radelta.imagej;

import com.sun.jna.Pointer;
import com.sun.jna.ptr.PointerByReference;

import java.io.Closeable;
import java.io.IOException;

final class RadeltaFile implements Closeable {
    private Pointer handle;
    final int width, height, slices, channels, frames;
    final int format, flags;
    final boolean lossy, streaming;

    private RadeltaFile(Pointer handle, RadeltaNative.FileInfo info) throws IOException {
        this.handle = handle;
        this.width = positive(info.nx, "X");
        this.height = positive(info.ny, "Y");
        this.slices = positive(info.nz, "Z");
        this.channels = positive(info.nc, "C");
        this.frames = positive(info.nt, "T");
        this.format = info.format;
        this.flags = info.flags;
        this.lossy = (flags & RadeltaNative.FLAG_LOSSY) != 0;
        this.streaming = (flags & RadeltaNative.FLAG_STREAMING) != 0;
        long plane = (long) width * (long) height;
        if (plane > Integer.MAX_VALUE) throw new IOException("A single Radelta plane exceeds Java array limits");
        long planes = (long) slices * channels * frames;
        if (planes > Integer.MAX_VALUE) throw new IOException("Radelta stack contains too many planes for ImageJ 1.x indexing");
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
            return new RadeltaFile(h, info);
        } catch (IOException e) {
            api.radelta_file_close(h);
            throw e;
        }
    }

    synchronized byte[] readMetadata() throws IOException {
        if (handle == null) throw new IOException("Radelta file is closed");
        RadeltaNative.SizeTReference size = new RadeltaNative.SizeTReference();
        int rc = RadeltaNative.api().radelta_file_get_metadata(handle, null, new RadeltaNative.SizeT(), size);
        if (rc != RadeltaNative.OK && rc != 1) throw new IOException("Could not read Radelta metadata");
        long length = size.value();
        if (length < 0 || length > Integer.MAX_VALUE) throw new IOException("Metadata exceeds Java array limits");
        if (length == 0) return new byte[0];
        byte[] data = new byte[(int)length];
        rc = RadeltaNative.api().radelta_file_get_metadata(handle, data, new RadeltaNative.SizeT(length), size);
        if (rc != RadeltaNative.OK || size.value() != length) throw new IOException("Could not read Radelta metadata");
        return data;
    }

    synchronized short[] readPlane(int t, int c, int z) throws IOException {
        if (handle == null) throw new IOException("Radelta file is closed");
        short[] pixels = new short[Math.multiplyExact(width, height)];
        int rc = RadeltaNative.api().radelta_file_read_plane_u16(
                handle, t, c, z, pixels, pixels.length);
        if (rc != RadeltaNative.OK) {
            throw new IOException("Radelta decode failed: " + RadeltaNative.lastError());
        }
        return pixels;
    }

    int planeCount() {
        return Math.multiplyExact(Math.multiplyExact(channels, slices), frames);
    }

    @Override
    public synchronized void close() {
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
