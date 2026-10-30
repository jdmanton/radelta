/* BSD-3-Clause; see ../../LICENSE. Create fixtures with the full SDK filter or
 * the embedded filter, then read them in a fresh process using the other one
 * (or Fiji/JHDF5). The embedded filter has no set_local callback, so its
 * writer supplies every filter parameter, exactly as the ImageJ plugin does. */
#include <hdf5.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#ifndef RADELTA_HDF5_FILTER_ID
#define RADELTA_HDF5_FILTER_ID 65024
#endif
#define CHECK(x) do { if (!(x)) { fprintf(stderr, "Failed: %s (line %d)\n", #x, __LINE__); exit(1); } } while (0)
#define OPTIONS_COUNT 11
#define LOCAL_COUNT 17
#define EMBEDDED_SUFFIX ", embedded)"

static const char *names[] = {"lossless2d_le", "lossless3d_be", "lossless4d_le",
    "lossless5d_be", "lossy5d_le", "lossy3d_be"};
static const int ranks[] = {2, 3, 4, 5, 5, 3};

/* Name of the registered filter, as the ImageJ plugin reads it. */
static void filter_name(char *name, size_t size) {
    unsigned flags, values[LOCAL_COUNT];
    size_t count = LOCAL_COUNT;
    hid_t dcpl = H5Pcreate(H5P_DATASET_CREATE);
    CHECK(dcpl >= 0);
    CHECK(H5Pset_filter(dcpl, RADELTA_HDF5_FILTER_ID, H5Z_FLAG_OPTIONAL, 0, NULL) >= 0);
    name[0] = 0;
    /* By index, as the ImageJ plugin must: JHDF5's by-id binding omits names. */
    CHECK(H5Pget_filter2(dcpl, 0, &flags, &count, values, size, name, NULL) == RADELTA_HDF5_FILTER_ID);
    CHECK(H5Pclose(dcpl) >= 0);
}

static int ends_with(const char *text, const char *suffix) {
    size_t n = strlen(text), m = strlen(suffix);
    return n >= m && !strcmp(text + n - m, suffix);
}

/* Explicit local parameters: chunk extents X,Y,Z,C,T padded with 1, then
 * the file byte order. The SDK filter derives these in set_local. */
static void explicit_parameters(unsigned *p, int rank, const hsize_t *chunks, int big) {
    int i;
    for (i = 0; i < 5; ++i) p[OPTIONS_COUNT+i] = 1;
    for (i = 0; i < rank; ++i) p[OPTIONS_COUNT+i] = (unsigned)chunks[rank-1-i];
    p[LOCAL_COUNT-1] = (unsigned)big;
}

/* A write through the embedded filter with only the user options must fail:
 * the filter cannot learn the chunk geometry without set_local. Without a
 * chunk cache, HDF5 filters each chunk during H5Dwrite, so it reports this. */
static void check_incomplete_parameters_fail(hid_t file) {
    const hsize_t shape[2] = {13, 17}, chunks[2] = {7, 9};
    unsigned options[OPTIONS_COUNT] = {1, 0, 4, 2, 10};
    uint16_t input[13*17] = {0};
    hid_t space = H5Screate_simple(2, shape, NULL);
    hid_t dcpl = H5Pcreate(H5P_DATASET_CREATE), dapl = H5Pcreate(H5P_DATASET_ACCESS), ds;
    herr_t status;
    CHECK(space >= 0 && dcpl >= 0 && dapl >= 0);
    CHECK(H5Pset_chunk(dcpl, 2, chunks) >= 0);
    CHECK(H5Pset_filter(dcpl, RADELTA_HDF5_FILTER_ID, H5Z_FLAG_MANDATORY, OPTIONS_COUNT, options) >= 0);
    CHECK(H5Pset_chunk_cache(dapl, 0, 0, 1.0) >= 0);
    ds = H5Dcreate2(file, "incomplete", H5T_STD_U16LE, space, H5P_DEFAULT, dcpl, dapl);
    CHECK(ds >= 0);
    H5E_BEGIN_TRY {
        status = H5Dwrite(ds, H5T_NATIVE_UINT16, H5S_ALL, H5S_ALL, H5P_DEFAULT, input);
    } H5E_END_TRY;
    CHECK(status < 0);
    CHECK(H5Dclose(ds) >= 0);
    CHECK(H5Pclose(dapl) >= 0); CHECK(H5Pclose(dcpl) >= 0); CHECK(H5Sclose(space) >= 0);
    CHECK(H5Ldelete(file, "incomplete", H5P_DEFAULT) >= 0);
    puts("Embedded filter rejected a write without explicit chunk parameters");
}

static void write_fixture(const char *path, int embedded) {
    const hsize_t shape[5] = {2, 2, 5, 13, 17}, chunks[5] = {2, 2, 3, 7, 9};
    const float spacing[3] = {2.5f, 0.75f, 0.5f};
    hid_t file, group;
    unsigned config;
    size_t k;
    CHECK(H5Zfilter_avail(RADELTA_HDF5_FILTER_ID) > 0);
    CHECK(H5Zget_filter_info(RADELTA_HDF5_FILTER_ID, &config) >= 0);
    CHECK(config & H5Z_FILTER_CONFIG_ENCODE_ENABLED);
    if (embedded) {
        char name[128];
        filter_name(name, sizeof(name));
        CHECK(ends_with(name, EMBEDDED_SUFFIX));
    }
    file = H5Fcreate(path, H5F_ACC_EXCL, H5P_DEFAULT, H5P_DEFAULT); CHECK(file >= 0);
    if (embedded) check_incomplete_parameters_fail(file);
    group = H5Gcreate2(file, "expected", H5P_DEFAULT, H5P_DEFAULT, H5P_DEFAULT); CHECK(group >= 0);
    for (k = 0; k < sizeof(ranks)/sizeof(ranks[0]); ++k) {
        int rank = ranks[k], big = (k == 1 || k == 3 || k == 5), lossy = k >= 4;
        size_t i, n = 1;
        unsigned options[LOCAL_COUNT] = {1, (unsigned)lossy, 4, 2, 10};
        double values[3] = {100.0, 0.46, 2.0};
        uint16_t *input, *reference;
        hid_t space, dcpl, ds, expected, attr_space, attr;
        hsize_t three = 3;
        for (i = (size_t)(5-rank); i < 5; ++i) n *= (size_t)shape[i];
        input = malloc(n * sizeof(*input)); reference = malloc(n * sizeof(*reference));
        CHECK(input && reference);
        for (i = 0; i < n; ++i) input[i] = (uint16_t)((i * 17 + 32760) % 65536);
        for (i = 0; i < 3; ++i) {
            uint64_t bits; memcpy(&bits, &values[i], sizeof(bits));
            options[5+2*i] = (unsigned)bits; options[6+2*i] = (unsigned)(bits >> 32);
        }
        space = H5Screate_simple(rank, shape+5-rank, NULL); CHECK(space >= 0);
        dcpl = H5Pcreate(H5P_DATASET_CREATE); CHECK(dcpl >= 0);
        CHECK(H5Pset_chunk(dcpl, rank, chunks+5-rank) >= 0);
        if (embedded) {
            explicit_parameters(options, rank, chunks+5-rank, big);
            CHECK(H5Pset_filter(dcpl, RADELTA_HDF5_FILTER_ID, H5Z_FLAG_MANDATORY,
                LOCAL_COUNT, options) >= 0);
        } else {
            CHECK(H5Pset_filter(dcpl, RADELTA_HDF5_FILTER_ID, H5Z_FLAG_MANDATORY,
                lossy ? OPTIONS_COUNT : 0, lossy ? options : NULL) >= 0);
        }
        ds = H5Dcreate2(file, names[k], big ? H5T_STD_U16BE : H5T_STD_U16LE,
            space, H5P_DEFAULT, dcpl, H5P_DEFAULT); CHECK(ds >= 0);
        CHECK(H5Dwrite(ds, H5T_NATIVE_UINT16, H5S_ALL, H5S_ALL, H5P_DEFAULT, input) >= 0);
        /* Evict the unquantised write cache before deriving the reference. */
        CHECK(H5Dclose(ds) >= 0);
        ds = H5Dopen2(file, names[k], H5P_DEFAULT); CHECK(ds >= 0);
        CHECK(H5Dread(ds, H5T_NATIVE_UINT16, H5S_ALL, H5S_ALL, H5P_DEFAULT, reference) >= 0);
        CHECK(lossy ? memcmp(input, reference, n*2) != 0 : memcmp(input, reference, n*2) == 0);
        expected = H5Dcreate2(group, names[k], H5T_STD_U16LE, space,
            H5P_DEFAULT, H5P_DEFAULT, H5P_DEFAULT); CHECK(expected >= 0);
        CHECK(H5Dwrite(expected, H5T_NATIVE_UINT16, H5S_ALL, H5S_ALL, H5P_DEFAULT, reference) >= 0);
        attr_space = H5Screate_simple(1, &three, NULL); CHECK(attr_space >= 0);
        attr = H5Acreate2(ds, "element_size_um", H5T_IEEE_F32LE, attr_space, H5P_DEFAULT, H5P_DEFAULT);
        CHECK(attr >= 0); CHECK(H5Awrite(attr, H5T_NATIVE_FLOAT, spacing) >= 0);
        CHECK(H5Aclose(attr) >= 0); CHECK(H5Sclose(attr_space) >= 0);
        CHECK(H5Dclose(expected) >= 0); CHECK(H5Dclose(ds) >= 0);
        CHECK(H5Pclose(dcpl) >= 0); CHECK(H5Sclose(space) >= 0);
        free(input); free(reference);
    }
    CHECK(H5Gclose(group) >= 0); CHECK(H5Fclose(file) >= 0);
    printf("Created lossless/lossy fixtures with the %s filter, ranks 2-5 and both byte orders\n",
        embedded ? "embedded" : "SDK");
}

static void read_fixture(const char *path, int embedded) {
    hid_t file;
    unsigned config;
    size_t k;
    CHECK(H5Zfilter_avail(RADELTA_HDF5_FILTER_ID) > 0);
    CHECK(H5Zget_filter_info(RADELTA_HDF5_FILTER_ID, &config) >= 0);
    CHECK(config & H5Z_FILTER_CONFIG_DECODE_ENABLED);
    if (embedded) {
        char name[128];
        CHECK(config & H5Z_FILTER_CONFIG_ENCODE_ENABLED);
        filter_name(name, sizeof(name));
        CHECK(ends_with(name, EMBEDDED_SUFFIX));
    }
    file = H5Fopen(path, H5F_ACC_RDONLY, H5P_DEFAULT); CHECK(file >= 0);
    for (k = 0; k < sizeof(ranks)/sizeof(ranks[0]); ++k) {
        char expected_path[80];
        hid_t ds, expected, space;
        hssize_t n;
        uint16_t *output, *reference;
        snprintf(expected_path, sizeof(expected_path), "expected/%s", names[k]);
        ds = H5Dopen2(file, names[k], H5P_DEFAULT); CHECK(ds >= 0);
        expected = H5Dopen2(file, expected_path, H5P_DEFAULT); CHECK(expected >= 0);
        space = H5Dget_space(ds); CHECK(space >= 0);
        n = H5Sget_simple_extent_npoints(space); CHECK(n > 0);
        output = malloc((size_t)n * 2); reference = malloc((size_t)n * 2); CHECK(output && reference);
        CHECK(H5Dread(ds, H5T_NATIVE_UINT16, H5S_ALL, H5S_ALL, H5P_DEFAULT, output) >= 0);
        CHECK(H5Dread(expected, H5T_NATIVE_UINT16, H5S_ALL, H5S_ALL, H5P_DEFAULT, reference) >= 0);
        CHECK(memcmp(output, reference, (size_t)n * 2) == 0);
        free(output); free(reference);
        CHECK(H5Sclose(space) >= 0); CHECK(H5Dclose(expected) >= 0); CHECK(H5Dclose(ds) >= 0);
    }
    CHECK(H5Fclose(file) >= 0);
    printf("%s filter: all fixtures passed\n", embedded ? "Embedded" : "SDK");
}

/* Compare a dataset written elsewhere (by the ImageJ plugin) with raw
 * little-endian uint16 pixels, decoding through the filter on the plugin path. */
static void compare_raw(const char *path, const char *dataset, const char *raw_path) {
    FILE *raw = fopen(raw_path, "rb");
    hid_t file, ds, space;
    hssize_t n;
    uint16_t *output;
    unsigned char *bytes;
    size_t i;
    CHECK(raw != NULL);
    file = H5Fopen(path, H5F_ACC_RDONLY, H5P_DEFAULT); CHECK(file >= 0);
    ds = H5Dopen2(file, dataset, H5P_DEFAULT); CHECK(ds >= 0);
    space = H5Dget_space(ds); CHECK(space >= 0);
    n = H5Sget_simple_extent_npoints(space); CHECK(n > 0);
    output = malloc((size_t)n * 2); bytes = malloc((size_t)n * 2 + 1); CHECK(output && bytes);
    CHECK(fread(bytes, 1, (size_t)n * 2 + 1, raw) == (size_t)n * 2);
    CHECK(H5Dread(ds, H5T_NATIVE_UINT16, H5S_ALL, H5S_ALL, H5P_DEFAULT, output) >= 0);
    for (i = 0; i < (size_t)n; ++i)
        CHECK(output[i] == (uint16_t)(bytes[2*i] | (bytes[2*i+1] << 8)));
    free(output); free(bytes); fclose(raw);
    CHECK(H5Sclose(space) >= 0); CHECK(H5Dclose(ds) >= 0); CHECK(H5Fclose(file) >= 0);
    printf("%s%s: matches its expected pixels\n", path, dataset);
}

/* write/read use the SDK and embedded filter respectively;
 * write-embedded/read-sdk check the other direction. */
int main(int argc, char **argv) {
    if (argc == 5 && !strcmp(argv[1], "compare")) {
        compare_raw(argv[2], argv[3], argv[4]);
        return 0;
    }
    CHECK(argc == 3);
    if (!strcmp(argv[1], "write")) write_fixture(argv[2], 0);
    else if (!strcmp(argv[1], "write-embedded")) write_fixture(argv[2], 1);
    else if (!strcmp(argv[1], "read-sdk")) read_fixture(argv[2], 0);
    else { CHECK(!strcmp(argv[1], "read")); read_fixture(argv[2], 1); }
    return 0;
}
