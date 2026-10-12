#include <hdf5.h>
#include <stdio.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#ifndef RADELTA_HDF5_FILTER_ID
#define RADELTA_HDF5_FILTER_ID 65024
#endif
#define CHECK(expr) do { if (!(expr)) { fprintf(stderr, "Failed at %s:%d: %s\n", __FILE__, __LINE__, #expr); return 1; } } while (0)
int main(void) {
    hsize_t shape[5] = {2, 2, 5, 13, 17}, chunks[5] = {1, 1, 3, 7, 9};
    hsize_t start[5] = {1, 1, 2, 4, 5}, size[5] = {1, 1, 1, 3, 4};
    uint16_t input[2*2*5*13*17], output[2*2*5*13*17], roi[12];
    hid_t file, space, dcpl, ds, memory;
    size_t i;
    for (i=0; i<sizeof(input)/sizeof(input[0]); ++i) input[i] = (uint16_t)((i*17) % 65536);
    CHECK(H5Zfilter_avail(RADELTA_HDF5_FILTER_ID) > 0);
    space = H5Screate_simple(5, shape, NULL); CHECK(space >= 0);
    dcpl = H5Pcreate(H5P_DATASET_CREATE); CHECK(dcpl >= 0);
    CHECK(H5Pset_chunk(dcpl, 5, chunks) >= 0);
    CHECK(H5Pset_filter(dcpl, RADELTA_HDF5_FILTER_ID, H5Z_FLAG_MANDATORY, 0, NULL) >= 0);
    file = H5Fcreate("roundtrip.h5", H5F_ACC_EXCL, H5P_DEFAULT, H5P_DEFAULT); CHECK(file >= 0);
    ds = H5Dcreate2(file, "images", H5T_STD_U16BE, space, H5P_DEFAULT, dcpl, H5P_DEFAULT); CHECK(ds >= 0);
    CHECK(H5Dwrite(ds, H5T_NATIVE_UINT16, H5S_ALL, H5S_ALL, H5P_DEFAULT, input) >= 0);
    CHECK(H5Dclose(ds) >= 0); CHECK(H5Fclose(file) >= 0);
    file = H5Fopen("roundtrip.h5", H5F_ACC_RDONLY, H5P_DEFAULT); CHECK(file >= 0);
    ds = H5Dopen2(file, "images", H5P_DEFAULT); CHECK(ds >= 0);
    CHECK(H5Dread(ds, H5T_NATIVE_UINT16, H5S_ALL, H5S_ALL, H5P_DEFAULT, output) >= 0);
    CHECK(memcmp(input, output, sizeof(input)) == 0);
    CHECK(H5Sselect_hyperslab(space, H5S_SELECT_SET, start, NULL, size, NULL) >= 0);
    memory = H5Screate_simple(5, size, NULL); CHECK(memory >= 0);
    CHECK(H5Dread(ds, H5T_NATIVE_UINT16, memory, space, H5P_DEFAULT, roi) >= 0);
    for (i=0; i<12; ++i) CHECK(roi[i] == input[((((1*2+1)*5+2)*13+4+i/4)*17+5+i%4)]);
    CHECK(H5Sclose(memory) >= 0); CHECK(H5Dclose(ds) >= 0);
    CHECK(H5Fclose(file) >= 0); CHECK(H5Sclose(space) >= 0); CHECK(H5Pclose(dcpl) >= 0);
    remove("roundtrip.h5");
    puts("HDF5 plugin loading, big-endian 5D round trip, edge chunks and hyperslab passed");
    return 0;
}
