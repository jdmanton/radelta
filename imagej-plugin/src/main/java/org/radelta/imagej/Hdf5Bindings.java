package org.radelta.imagej;

import java.io.IOException;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.charset.StandardCharsets;
import java.util.Arrays;
import java.util.HashMap;
import java.util.Map;

/**
 * Reflective access to the reader's own {@code hdf.hdf5lib} bindings.
 *
 * <p>The plugin must load without HDF5 installed, and it must use the HDF5
 * runtime that JHDF5 embeds rather than linking a second one, so it never
 * compiles against those classes. Each wrapper converts binding failures into
 * {@link IOException}s naming the HDF5 call.
 */
final class Hdf5Bindings {
    private final Class<?> h5;
    private final Class<?> constants;
    private final Map<String, Method> methods = new HashMap<String, Method>();

    Hdf5Bindings() throws IOException {
        try {
            h5 = RadeltaHdf5.readerClass("hdf.hdf5lib.H5");
            constants = RadeltaHdf5.readerClass("hdf.hdf5lib.HDF5Constants");
        } catch (ClassNotFoundException | LinkageError e) {
            throw new IOException("The HDF5 reader is missing. Install the Radelta Fiji bundle and restart Fiji.", e);
        }
    }

    long constant(String name) throws IOException {
        try {
            return ((Number) constants.getField(name).get(null)).longValue();
        } catch (ReflectiveOperationException | LinkageError e) {
            throw new IOException("Incompatible HDF5 reader: constant " + name + " is unavailable", e);
        }
    }

    int intConstant(String name) throws IOException {
        return (int) constant(name);
    }

    private Object call(String name, Class<?>[] types, Object... args) throws IOException {
        String key = name + Arrays.toString(types);
        Method method = methods.get(key);
        try {
            if (method == null) {
                method = h5.getMethod(name, types);
                methods.put(key, method);
            }
            return method.invoke(null, args);
        } catch (InvocationTargetException e) {
            Throwable cause = e.getCause() == null ? e : e.getCause();
            String detail = cause.getMessage();
            throw new IOException(name + " failed" + (detail == null || detail.trim().isEmpty() ? "" : ": " + detail.trim()), cause);
        } catch (ReflectiveOperationException | LinkageError e) {
            throw new IOException("Incompatible HDF5 reader: " + name + " is unavailable", e);
        }
    }

    private static Class<?>[] types(Class<?>... types) {
        return types;
    }

    private long callLong(String name, Class<?>[] types, Object... args) throws IOException {
        long id = ((Number) call(name, types, args)).longValue();
        if (id < 0) throw new IOException(name + " failed");
        return id;
    }

    private void callStatus(String name, Class<?>[] types, Object... args) throws IOException {
        Object result = call(name, types, args);
        if (result instanceof Number && ((Number) result).intValue() < 0) throw new IOException(name + " failed");
    }

    // Library-wide calls.

    void pluginPrepend(String directory) throws IOException {
        call("H5PLprepend", types(String.class), directory);
    }

    int filterAvailable(int filter) throws IOException {
        return ((Number) call("H5Zfilter_avail", types(int.class), filter)).intValue();
    }

    int filterInfo(int filter) throws IOException {
        return ((Number) call("H5Zget_filter_info", types(int.class), filter)).intValue();
    }

    /** The name HDF5 reports for a registered filter, as stored in its class. */
    String filterName(int filter) throws IOException {
        long dcpl = createPropertyList("H5P_DATASET_CREATE");
        try {
            callStatus("H5Pset_filter", types(long.class, int.class, int.class, long.class, int[].class),
                    dcpl, filter, intConstant("H5Z_FLAG_OPTIONAL"), 0L, new int[0]);
            // JHDF5's H5Pget_filter_by_id binding never fills in the name, so
            // read the only filter in this pipeline by index instead.
            String[] name = new String[1];
            Object id = call("H5Pget_filter", types(long.class, int.class, int[].class, long[].class, int[].class,
                    long.class, String[].class, int[].class),
                    dcpl, 0, new int[1], new long[] {0}, new int[1], 256L, name, new int[1]);
            if (((Number) id).intValue() != filter) throw new IOException("H5Pget_filter returned another filter");
            return name[0] == null ? "" : name[0];
        } finally {
            closePropertyList(dcpl);
        }
    }

    // Files, groups and property lists.

    long createFile(String path) throws IOException {
        long fileDefault = constant("H5P_DEFAULT");
        return callLong("H5Fcreate", types(String.class, int.class, long.class, long.class),
                path, intConstant("H5F_ACC_TRUNC"), fileDefault, fileDefault);
    }

    void closeFile(long file) throws IOException {
        callStatus("H5Fclose", types(long.class), file);
    }

    // Reading the structure of existing files.

    boolean isHdf5(String path) throws IOException {
        return (Boolean) call("H5Fis_hdf5", types(String.class), path);
    }

    long openFileReadOnly(String path) throws IOException {
        return callLong("H5Fopen", types(String.class, int.class, long.class),
                path, intConstant("H5F_ACC_RDONLY"), constant("H5P_DEFAULT"));
    }

    /** Number of links in a group, which is opened and closed here. */
    long linkCount(long file, String group) throws IOException {
        long id = callLong("H5Gopen", types(long.class, String.class, long.class), file, group, constant("H5P_DEFAULT"));
        try {
            Object info = call("H5Gget_info", types(long.class), id);
            return info.getClass().getField("nlinks").getLong(info);
        } catch (ReflectiveOperationException e) {
            throw new IOException("Incompatible HDF5 reader: H5G_info_t.nlinks is unavailable", e);
        } finally {
            callStatus("H5Gclose", types(long.class), id);
        }
    }

    /** The name of the index-th link in a group, in name order. */
    String linkName(long file, String group, long index) throws IOException {
        return (String) call("H5Lget_name_by_idx",
                types(long.class, String.class, int.class, int.class, long.class, long.class),
                file, group, intConstant("H5_INDEX_NAME"), intConstant("H5_ITER_INC"), index, constant("H5P_DEFAULT"));
    }

    /** Object type and a file-unique address, following soft links. */
    long[] objectTypeAndAddress(long file, String path) throws IOException {
        Object info = call("H5Oget_info_by_name", types(long.class, String.class, int.class, long.class),
                file, path, intConstant("H5O_INFO_BASIC"), constant("H5P_DEFAULT"));
        try {
            return new long[] {info.getClass().getField("type").getInt(info), info.getClass().getField("addr").getLong(info)};
        } catch (ReflectiveOperationException e) {
            throw new IOException("Incompatible HDF5 reader: H5O_info_t fields are unavailable", e);
        }
    }

    long openDataset(long file, String path) throws IOException {
        return callLong("H5Dopen", types(long.class, String.class, long.class), file, path, constant("H5P_DEFAULT"));
    }

    /** Filter identifiers in a dataset's pipeline, in order. */
    int[] datasetFilters(long dataset) throws IOException {
        long dcpl = callLong("H5Dget_create_plist", types(long.class), dataset);
        try {
            int count = ((Number) call("H5Pget_nfilters", types(long.class), dcpl)).intValue();
            int[] filters = new int[Math.max(0, count)];
            for (int i = 0; i < filters.length; ++i) {
                filters[i] = ((Number) call("H5Pget_filter", types(long.class, int.class, int[].class, long[].class,
                        int[].class, long.class, String[].class, int[].class),
                        dcpl, i, new int[1], new long[] {0}, new int[1], 64L, new String[1], new int[1])).intValue();
            }
            return filters;
        } finally {
            closePropertyList(dcpl);
        }
    }

    long[] datasetShape(long dataset) throws IOException {
        long space = callLong("H5Dget_space", types(long.class), dataset);
        try {
            int rank = ((Number) call("H5Sget_simple_extent_ndims", types(long.class), space)).intValue();
            long[] dims = new long[Math.max(0, rank)];
            callStatus("H5Sget_simple_extent_dims", types(long.class, long[].class, long[].class), space, dims, null);
            return dims;
        } finally {
            closeSpace(space);
        }
    }

    /**
     * A string attribute, fixed or variable length (h5py writes the latter),
     * or null when it is absent or not a string.
     */
    String readStringAttribute(long file, String object, String name) throws IOException {
        long lapl = constant("H5P_DEFAULT");
        if (!(Boolean) call("H5Aexists_by_name", types(long.class, String.class, String.class, long.class),
                file, object, name, lapl)) return null;
        long attribute = callLong("H5Aopen_by_name", types(long.class, String.class, String.class, long.class, long.class),
                file, object, name, lapl, lapl);
        try {
            long type = callLong("H5Aget_type", types(long.class), attribute);
            try {
                if (((Number) call("H5Tget_class", types(long.class), type)).intValue() != intConstant("H5T_STRING")) return null;
                if ((Boolean) call("H5Tis_variable_str", types(long.class), type)) {
                    String[] value = new String[1];
                    callStatus("H5AreadVL", types(long.class, long.class, String[].class), attribute, type, value);
                    return value[0];
                }
                long size = ((Number) call("H5Tget_size", types(long.class), type)).longValue();
                if (size <= 0 || size > 4096) return null;
                byte[] bytes = new byte[(int) size];
                callStatus("H5Aread", types(long.class, long.class, byte[].class), attribute, type, bytes);
                int end = 0;
                while (end < bytes.length && bytes[end] != 0) ++end;
                return new String(bytes, 0, end, StandardCharsets.UTF_8);
            } finally {
                callStatus("H5Tclose", types(long.class), type);
            }
        } finally {
            callStatus("H5Aclose", types(long.class), attribute);
        }
    }

    long createPropertyList(String className) throws IOException {
        return callLong("H5Pcreate", types(long.class), constant(className));
    }

    void closePropertyList(long plist) throws IOException {
        callStatus("H5Pclose", types(long.class), plist);
    }

    void createIntermediateGroups(long lcpl) throws IOException {
        callStatus("H5Pset_create_intermediate_group", types(long.class, boolean.class), lcpl, true);
    }

    void setChunk(long dcpl, long[] chunks) throws IOException {
        callStatus("H5Pset_chunk", types(long.class, int.class, long[].class), dcpl, chunks.length, chunks);
    }

    void setFilter(long dcpl, int filter, int[] parameters) throws IOException {
        callStatus("H5Pset_filter", types(long.class, int.class, int.class, long.class, int[].class),
                dcpl, filter, intConstant("H5Z_FLAG_MANDATORY"), (long) parameters.length, parameters);
    }

    // Dataspaces, datasets and attributes.

    long createSimpleSpace(long[] dims) throws IOException {
        return callLong("H5Screate_simple", types(int.class, long[].class, long[].class),
                dims.length, dims, dims.clone());
    }

    long createScalarSpace() throws IOException {
        return callLong("H5Screate", types(int.class), intConstant("H5S_SCALAR"));
    }

    void closeSpace(long space) throws IOException {
        callStatus("H5Sclose", types(long.class), space);
    }

    void selectHyperslab(long space, long[] start, long[] count) throws IOException {
        long[] ones = new long[start.length];
        Arrays.fill(ones, 1);
        callStatus("H5Sselect_hyperslab", types(long.class, int.class, long[].class, long[].class, long[].class, long[].class),
                space, intConstant("H5S_SELECT_SET"), start, ones, count, ones);
    }

    long createDataset(long file, String name, long type, long space, long lcpl, long dcpl) throws IOException {
        return callLong("H5Dcreate", types(long.class, String.class, long.class, long.class, long.class, long.class, long.class),
                file, name, type, space, lcpl, dcpl, constant("H5P_DEFAULT"));
    }

    void writeUint16(long dataset, long memorySpace, long fileSpace, short[] pixels) throws IOException {
        callStatus("H5Dwrite_short", types(long.class, long.class, long.class, long.class, long.class, short[].class),
                dataset, constant("H5T_NATIVE_UINT16"), memorySpace, fileSpace, constant("H5P_DEFAULT"), pixels);
    }

    void closeDataset(long dataset) throws IOException {
        callStatus("H5Dclose", types(long.class), dataset);
    }

    private void writeAttribute(long object, String name, long fileType, long space, long memoryType, byte[] data)
            throws IOException {
        long attribute = callLong("H5Acreate", types(long.class, String.class, long.class, long.class, long.class, long.class),
                object, name, fileType, space, constant("H5P_DEFAULT"), constant("H5P_DEFAULT"));
        try {
            callStatus("H5Awrite", types(long.class, long.class, byte[].class), attribute, memoryType, data);
        } finally {
            callStatus("H5Aclose", types(long.class), attribute);
        }
    }

    /** A fixed-length ASCII string attribute, readable by h5py and HDF5 tools. */
    void writeStringAttribute(long object, String name, String value) throws IOException {
        byte[] bytes = (value + "\0").getBytes(StandardCharsets.US_ASCII);
        long type = callLong("H5Tcopy", types(long.class), constant("H5T_C_S1"));
        try {
            callStatus("H5Tset_size", types(long.class, long.class), type, (long) bytes.length);
            long space = createScalarSpace();
            try {
                writeAttribute(object, name, type, space, type, bytes);
            } finally {
                closeSpace(space);
            }
        } finally {
            callStatus("H5Tclose", types(long.class), type);
        }
    }

    void writeDoubleAttribute(long object, String name, double value) throws IOException {
        long space = createScalarSpace();
        try {
            byte[] bytes = ByteBuffer.allocate(8).order(ByteOrder.nativeOrder()).putDouble(value).array();
            writeAttribute(object, name, constant("H5T_IEEE_F64LE"), space, constant("H5T_NATIVE_DOUBLE"), bytes);
        } finally {
            closeSpace(space);
        }
    }

    void writeUint8Attribute(long object, String name, int value) throws IOException {
        long space = createScalarSpace();
        try {
            writeAttribute(object, name, constant("H5T_STD_U8LE"), space, constant("H5T_NATIVE_UINT8"),
                    new byte[] {(byte) value});
        } finally {
            closeSpace(space);
        }
    }

    void writeFloatArrayAttribute(long object, String name, float[] values) throws IOException {
        long space = createSimpleSpace(new long[] {values.length});
        try {
            ByteBuffer buffer = ByteBuffer.allocate(4 * values.length).order(ByteOrder.nativeOrder());
            for (float value : values) buffer.putFloat(value);
            writeAttribute(object, name, constant("H5T_IEEE_F32LE"), space, constant("H5T_NATIVE_FLOAT"), buffer.array());
        } finally {
            closeSpace(space);
        }
    }
}
