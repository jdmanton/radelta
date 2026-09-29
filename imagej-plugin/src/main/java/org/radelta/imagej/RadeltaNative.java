package org.radelta.imagej;

import com.sun.jna.Library;
import com.sun.jna.IntegerType;
import com.sun.jna.ptr.ByReference;
import com.sun.jna.Memory;
import com.sun.jna.Native;
import com.sun.jna.Pointer;
import com.sun.jna.Structure;
import com.sun.jna.ptr.PointerByReference;

import java.io.IOException;
import java.io.InputStream;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardCopyOption;
import java.util.Arrays;
import java.util.HashMap;
import java.util.List;
import java.util.Map;

final class RadeltaNative {
    static final int OK = 0;
    static final int FLAG_LOSSY = 1;
    static final int FLAG_MULTIDIMENSIONAL = 2;
    static final int FLAG_STREAMING = 4;

    interface Api extends Library {
        SizeT radelta_get_metadata_limit();
        int radelta_file_get_metadata(Pointer handle, byte[] output, SizeT capacity, SizeTReference size);
        int radelta_writer_set_metadata(Pointer handle, byte[] data, SizeT size);
        int radelta_file_open(String pathUtf8, PointerByReference outHandle);
        void radelta_file_close(Pointer handle);
        int radelta_file_get_info(Pointer handle, FileInfo outInfo);
        int radelta_file_read_plane_u16(Pointer handle, int t, int c, int z,
                                        short[] output, long outputCapacityVoxels);

        int radelta_writer_create_lossless_u16(String pathUtf8,
                                               int nx, int ny, int nz, int nc, int nt,
                                               int memoryMib,
                                               PointerByReference outHandle);
        int radelta_writer_create_lossy_u16(String pathUtf8,
                                            int nx, int ny, int nz, int nc, int nt,
                                            double offsetAdu, double gainEPerAdu, double noiseStep,
                                            int memoryMib,
                                            PointerByReference outHandle);
        int radelta_writer_write_plane_u16(Pointer handle, int t, int c, int z,
                                           short[] input, long inputVoxels);
        int radelta_writer_finish(Pointer handle);
        void radelta_writer_close(Pointer handle);

        long radelta_file_last_error(Pointer buffer, long capacity);
    }

    public static class SizeT extends IntegerType {
        public SizeT() { this(0); }
        public SizeT(long value) { super(Native.SIZE_T_SIZE, value, true); }
    }
    public static class SizeTReference extends ByReference {
        public SizeTReference() { super(Native.SIZE_T_SIZE); }
        long value() { return Native.SIZE_T_SIZE == 8 ? getPointer().getLong(0) : Integer.toUnsignedLong(getPointer().getInt(0)); }
    }

    public static class FileInfo extends Structure {
        public int nx, ny, nz, nc, nt;
        public int format, flags;

        @Override
        protected List<String> getFieldOrder() {
            return Arrays.asList("nx", "ny", "nz", "nc", "nt", "format", "flags");
        }
    }

    private static volatile Api api;

    static Api api() {
        Api a = api;
        if (a != null) return a;
        synchronized (RadeltaNative.class) {
            if (api == null) api = load();
            return api;
        }
    }

    static String lastError() {
        Api a = api();
        long needed = a.radelta_file_last_error(Pointer.NULL, 0);
        long cap = Math.max(256, Math.min(needed, 1L << 20));
        Memory m = new Memory(cap);
        a.radelta_file_last_error(m, cap);
        return m.getString(0, "UTF-8");
    }

    private static Api load() {
        String override = System.getProperty("radelta.native.path");
        if (override == null || override.trim().isEmpty()) {
            override = System.getenv("RADELTA_NATIVE_LIBRARY");
        }
        Map<String, Object> opts = new HashMap<String, Object>();
        opts.put(Library.OPTION_STRING_ENCODING, "UTF-8");
        if (override != null && !override.trim().isEmpty()) {
            return Native.load(override, Api.class, opts);
        }

        String resource = nativeResource();
        if (resource != null) {
            try (InputStream in = RadeltaNative.class.getResourceAsStream(resource)) {
                if (in != null) {
                    String filename = resource.substring(resource.lastIndexOf('/') + 1);
                    Path dir = Files.createTempDirectory("radelta-imagej-native-");
                    Path out = dir.resolve(filename);
                    Files.copy(in, out, StandardCopyOption.REPLACE_EXISTING);
                    out.toFile().deleteOnExit();
                    dir.toFile().deleteOnExit();
                    return Native.load(out.toAbsolutePath().toString(), Api.class, opts);
                }
            } catch (IOException e) {
                throw new UnsatisfiedLinkError("Could not extract bundled Radelta native library: " + e.getMessage());
            }
        }
        return Native.load("radelta", Api.class, opts);
    }

    private static String nativeResource() {
        String os = System.getProperty("os.name", "").toLowerCase();
        String arch = System.getProperty("os.arch", "").toLowerCase();
        boolean arm64 = arch.contains("aarch64") || arch.contains("arm64");
        boolean x64 = arch.contains("amd64") || arch.contains("x86_64");
        if (os.contains("win") && x64) return "/natives/windows-x86_64/radelta.dll";
        if (os.contains("linux") && x64) return "/natives/linux-x86_64/libradelta.so";
        if (os.contains("linux") && arm64) return "/natives/linux-aarch64/libradelta.so";
        if (os.contains("mac") && x64) return "/natives/macos-x86_64/libradelta.dylib";
        if (os.contains("mac") && arm64) return "/natives/macos-aarch64/libradelta.dylib";
        return null;
    }

    private RadeltaNative() {}
}
