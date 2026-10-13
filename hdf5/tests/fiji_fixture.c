/* BSD-3-Clause; see ../../LICENSE. Create fixtures with the full filter, then
 * read them in a fresh process using the standalone decoder or Fiji/JHDF5. */
#include <hdf5.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#ifndef RADELTA_HDF5_FILTER_ID
#define RADELTA_HDF5_FILTER_ID 65024
#endif
#define CHECK(x) do { if (!(x)) { fprintf(stderr, "Failed: %s (line %d)\n", #x, __LINE__); exit(1); } } while (0)

static const char *names[] = {"lossless2d_le", "lossless3d_be", "lossless4d_le",
    "lossless5d_be", "lossy5d_le", "lossy3d_be"};
static const int ranks[] = {2, 3, 4, 5, 5, 3};

static void write_fixture(const char *path) {
    const hsize_t shape[5] = {2, 2, 5, 13, 17}, chunks[5] = {2, 2, 3, 7, 9};
    const float spacing[3] = {2.5f, 0.75f, 0.5f};
    hid_t file, group;
    unsigned config;
    size_t k;
    CHECK(H5Zfilter_avail(RADELTA_HDF5_FILTER_ID) > 0);
    CHECK(H5Zget_filter_info(RADELTA_HDF5_FILTER_ID, &config) >= 0);
    CHECK(config & H5Z_FILTER_CONFIG_ENCODE_ENABLED);
    file = H5Fcreate(path, H5F_ACC_EXCL, H5P_DEFAULT, H5P_DEFAULT); CHECK(file >= 0);
    group = H5Gcreate2(file, "expected", H5P_DEFAULT, H5P_DEFAULT, H5P_DEFAULT); CHECK(group >= 0);
    for (k = 0; k < sizeof(ranks)/sizeof(ranks[0]); ++k) {
        int rank = ranks[k], big = (k == 1 || k == 3 || k == 5), lossy = k >= 4;
        size_t i, n = 1;
        unsigned options[11] = {1, 1, 4, 2, 10};
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
        CHECK(H5Pset_filter(dcpl, RADELTA_HDF5_FILTER_ID, H5Z_FLAG_MANDATORY,
            lossy ? 11 : 0, lossy ? options : NULL) >= 0);
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
    puts("Created lossless/lossy Fiji fixtures, ranks 2-5 and both byte orders");
}

static void read_fixture(const char *path) {
    hid_t file;
    unsigned config;
    size_t k;
    CHECK(H5Zfilter_avail(RADELTA_HDF5_FILTER_ID) > 0);
    CHECK(H5Zget_filter_info(RADELTA_HDF5_FILTER_ID, &config) >= 0);
    CHECK(config & H5Z_FILTER_CONFIG_DECODE_ENABLED);
    CHECK(!(config & H5Z_FILTER_CONFIG_ENCODE_ENABLED));
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
    puts("Standalone read-only filter: all fixtures passed");
}

int main(int argc, char **argv) {
    CHECK(argc == 3);
    if (!strcmp(argv[1], "write")) write_fixture(argv[2]);
    else { CHECK(!strcmp(argv[1], "read")); read_fixture(argv[2]); }
    return 0;
}
