/* BSD-3-Clause; see ../../LICENSE. */
#include <hdf5.h>
#include <H5PLextern.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <math.h>
#include "radelta.h"

#ifndef RADELTA_HDF5_FILTER_ID
#define RADELTA_HDF5_FILTER_ID 65024
#endif
#define OPTIONS_COUNT 11
#define LOCAL_COUNT 17
#define PARAM_VERSION 1

/* Fiji/JHDF5 embeds its own HDF5 library, so the embedded build makes no HDF5
 * API calls: it uses the shared system C runtime and ships no second HDF5
 * runtime. On Windows this target and supported JHDF5 19.04.1 both use the
 * shared UCRT. The ordinary SDK filter continues using HDF5's allocator.
 *
 * Without HDF5 API calls the embedded build has no can_apply/set_local
 * callbacks. It decodes any Radelta dataset, and encodes only when the
 * dataset's creator supplies all LOCAL_COUNT parameters itself, as the ImageJ
 * plugin does. Creating a dataset with only the OPTIONS_COUNT user options
 * leaves the chunk geometry unknown, so every write then fails. */
#ifdef RADELTA_HDF5_EMBEDDED
#define FILTER_ALLOC(n) malloc(n)
#define FILTER_FREE(p) free(p)
#else
#define FILTER_ALLOC(n) H5allocate_memory((n), 0)
#define FILTER_FREE(p) H5free_memory(p)
#endif

/* cd_values: version, lossy, block depth, context, scale bits,
 * offset/gain/noise as low/high uint32 pairs, X,Y,Z,C,T, byte order (0 LE/1 BE).
 * The SDK filter derives the last six words in set_local and never trusts the
 * caller's values. The embedded filter takes them from the dataset creator. */
static double unpack_double(const unsigned *p) {
    uint64_t bits = (uint64_t)p[0] | ((uint64_t)p[1] << 32);
    double value;
    memcpy(&value, &bits, sizeof(value));
    return value;
}

static int valid_options(const unsigned *p) {
    if (p[0] != PARAM_VERSION || p[1] > 1 || p[3] > 3 ||
        (p[4] != 0 && (p[4] < 8 || p[4] > 11))) return 0;
    return p[1] == 0 || (isfinite(unpack_double(p+5)) &&
        isfinite(unpack_double(p+7)) && unpack_double(p+7) > 0 &&
        isfinite(unpack_double(p+9)) && unpack_double(p+9) > 0);
}

#ifndef RADELTA_HDF5_EMBEDDED
static htri_t can_apply(hid_t dcpl, hid_t type, hid_t space) {
    int rank = H5Sget_simple_extent_ndims(space);
    H5T_order_t order = H5Tget_order(type);
    unsigned flags;
    size_t count = 0;
    /* Radelta needs original pixel values; preceding byte transforms would
     * destroy the spatial model (shuffle, scaleoffset, another compressor). */
    if (H5Pget_nfilters(dcpl) < 1 ||
        H5Pget_filter2(dcpl, 0, &flags, &count, NULL, 0, NULL, NULL) !=
            RADELTA_HDF5_FILTER_ID) return -1;
    return rank >= 2 && rank <= 5 && H5Tget_class(type) == H5T_INTEGER &&
        H5Tget_size(type) == 2 && H5Tget_precision(type) == 16 &&
        H5Tget_offset(type) == 0 && H5Tget_sign(type) == H5T_SGN_NONE &&
        (order == H5T_ORDER_LE || order == H5T_ORDER_BE) ? 1 : -1;
}

static herr_t set_local(hid_t dcpl, hid_t type, hid_t space) {
    unsigned p[LOCAL_COUNT] = {1, 0, 4, 2, 10};
    unsigned flags = 0;
    size_t count = LOCAL_COUNT;
    hsize_t chunks[5];
    int rank, i;
    size_t voxels = 1;
    (void)space;
    if (H5Pget_filter_by_id2(dcpl, RADELTA_HDF5_FILTER_ID, &flags,
            &count, p, 0, NULL, NULL) < 0 ||
        (count != 0 && count != OPTIONS_COUNT && count != LOCAL_COUNT)) return -1;
    if (count == 0) {
        const unsigned defaults[OPTIONS_COUNT] = {1, 0, 4, 2, 10};
        memcpy(p, defaults, sizeof(defaults));
    }
    if (!valid_options(p)) return -1;
    rank = H5Pget_chunk(dcpl, 5, chunks);
    if (rank < 2 || rank > 5) return -1;
    for (i = 0; i < 5; ++i) p[11+i] = 1;
    for (i = 0; i < rank; ++i) {
        hsize_t n = chunks[rank-1-i];
        if (n == 0 || n > UINT32_MAX || n > SIZE_MAX / 2 / voxels) return -1;
        p[11+i] = (unsigned)n;
        voxels *= (size_t)n;
    }
    p[16] = H5Tget_order(type) == H5T_ORDER_BE;
    /* Make failures visible, including with h5py's optional custom filters.
     * A lossy encode must never silently skip quantisation. */
    return H5Pmodify_filter(dcpl, RADELTA_HDF5_FILTER_ID,
        flags & ~H5Z_FLAG_OPTIONAL, LOCAL_COUNT, p);
}
#endif

static void decode_pixels(const uint8_t *bytes, uint16_t *pixels, size_t n, int big) {
    size_t i;
    for (i = 0; i < n; ++i) pixels[i] = big ?
        (uint16_t)(((uint16_t)bytes[2*i] << 8) | bytes[2*i+1]) :
        (uint16_t)(bytes[2*i] | ((uint16_t)bytes[2*i+1] << 8));
}

static void encode_pixels(const uint16_t *pixels, uint8_t *bytes, size_t n, int big) {
    size_t i;
    for (i = 0; i < n; ++i) {
        uint16_t v = pixels[i];
        bytes[2*i+big] = (uint8_t)v;
        bytes[2*i+!big] = (uint8_t)(v >> 8);
    }
}

static size_t filter(unsigned flags, size_t count, const unsigned p[],
                     size_t nbytes, size_t *capacity, void **buffer) {
    size_t voxels = 1, i, bytes, result_size = 0;
    uint16_t *pixels = NULL;
    uint8_t *encoded = NULL;
    void *result = NULL;
    int32_t status;
    if (count != LOCAL_COUNT || !valid_options(p) || p[16] > 1 ||
        !capacity || !buffer || !*buffer || nbytes > *capacity) return 0;
    for (i = 11; i < 16; ++i) {
        if (!p[i] || p[i] > SIZE_MAX / 2 / voxels) return 0;
        voxels *= p[i];
    }
    bytes = voxels * 2;
    if (flags & H5Z_FLAG_REVERSE) {
        size_t needed = 0;
        if (nbytes < 4 || (p[1] ?
            (memcmp(*buffer, "RDLQ", 4) && memcmp(*buffer, "RDQ2", 4)) :
            (memcmp(*buffer, "RDL1", 4) && memcmp(*buffer, "RDM2", 4)))) return 0;
        uint32_t x=0, y=0, z=0, c=0, t=0;
        /* Inspect before allocating: damaged headers cannot dictate allocation
         * size, nor change the chunk geometry stored in the dataset. */
        status = radelta_decompress_u16_nd(*buffer, nbytes, NULL, 0,
            &needed, &x, &y, &z, &c, &t);
        if (status != RADELTA_BUFFER_TOO_SMALL || needed != voxels ||
            x != p[11] || y != p[12] || z != p[13] || c != p[14] || t != p[15]) return 0;
        pixels = FILTER_ALLOC(bytes);
        if (!pixels) return 0;
        status = radelta_decompress_u16_nd(*buffer, nbytes, pixels, voxels,
            &needed, &x, &y, &z, &c, &t);
        if (status != RADELTA_OK) goto failure;
        result = FILTER_ALLOC(bytes);
        if (!result) goto failure;
        encode_pixels(pixels, result, voxels, p[16]);
        result_size = bytes;
    } else {
        radelta_options opts = {p[2], p[3], p[4]};
        radelta_lossy_options lossy = {p[2], p[3], p[4],
            unpack_double(p+5), unpack_double(p+7), unpack_double(p+9)};
        if (nbytes != bytes) return 0;
        pixels = FILTER_ALLOC(bytes);
        if (!pixels) return 0;
        decode_pixels(*buffer, pixels, voxels, p[16]);
        status = radelta_compress_u16_nd_alloc(pixels, voxels,
            p[11], p[12], p[13], p[14], p[15],
            p[1] ? NULL : &opts, p[1] ? &lossy : NULL, &encoded, &result_size);
        if (status != RADELTA_OK) goto failure;
        result = FILTER_ALLOC(result_size);
        if (!result) goto failure;
        memcpy(result, encoded, result_size);
    }
    FILTER_FREE(pixels);
    radelta_buffer_free(encoded, encoded ? result_size : 0);
    FILTER_FREE(*buffer);
    *buffer = result;
    *capacity = result_size;
    return result_size;
failure:
    FILTER_FREE(pixels);
    FILTER_FREE(result);
    radelta_buffer_free(encoded, encoded ? result_size : 0);
    return 0;
}

static const H5Z_class2_t cls = {
#ifdef RADELTA_HDF5_EMBEDDED
    /* The ImageJ plugin identifies this build by the ", embedded)" suffix. */
    H5Z_CLASS_T_VERS, RADELTA_HDF5_FILTER_ID, 1, 1,
    "Radelta 0.8.9 (uint16 TCZYX, embedded)", NULL, NULL, filter
#else
    H5Z_CLASS_T_VERS, RADELTA_HDF5_FILTER_ID, 1, 1,
    "Radelta 0.8.9 (uint16 TCZYX)", can_apply, set_local, filter
#endif
};
H5PL_type_t H5PLget_plugin_type(void) { return H5PL_TYPE_FILTER; }
const void *H5PLget_plugin_info(void) { return &cls; }
