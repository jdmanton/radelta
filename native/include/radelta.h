#ifndef RADELTA_H
#define RADELTA_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

enum {
    RADELTA_OK = 0,
    RADELTA_BUFFER_TOO_SMALL = 1,
    RADELTA_INVALID_ARGUMENT = -1,
    RADELTA_CODEC_ERROR = -2,
    RADELTA_IO_ERROR = -3
};

enum {
    RADELTA_CONTEXT_SIGNED3 = 0,
    RADELTA_CONTEXT_SIGNS = 1,
    /* Default when options is NULL: fractional mean of causal neighbors. */
    RADELTA_CONTEXT_MEAN = 2,
    RADELTA_CONTEXT_MEAN_SIGNS = 3
};

typedef struct radelta_options {
    uint32_t block_depth;   /* 0 => default (4) */
    uint32_t context_mode;  /* RADELTA_CONTEXT_*; 0 is explicit SIGNED3 */
    uint32_t scale_bits;    /* 0 => default (10) */
} radelta_options;

/*
 * Camera-calibrated lossy mode.
 * gain_e_per_adu is electrons / ADU.
 *
 * electrons = max((ADU - offset_adu) * gain_e_per_adu, 0)
 * z         = 2*sqrt(electrons)
 * q         = round(z / noise_step)
 *
 * For shot-noise-limited data, z has approximately unit SD.  A noise_step
 * of 1 is relatively conservative; 2 is approximately round(sqrt(e-)).
 */
typedef struct radelta_lossy_options {
    uint32_t block_depth;       /* 0 => default (4) */
    uint32_t context_mode;      /* RADELTA_CONTEXT_*; 0 is explicit SIGNED3 */
    uint32_t scale_bits;        /* 0 => default (10) */
    double offset_adu;
    double gain_e_per_adu;
    double noise_step;
} radelta_lossy_options;

/* Compress a contiguous Z,Y,X uint16 volume losslessly. */
int32_t radelta_compress_u16(
    const uint16_t *input,
    size_t input_voxels,
    uint32_t nx,
    uint32_t ny,
    uint32_t nz,
    const radelta_options *options,
    uint8_t *output,
    size_t output_capacity,
    size_t *output_size
);

/* Compress a contiguous Z,Y,X uint16 volume using calibrated lossy mode. */
int32_t radelta_compress_lossy_u16(
    const uint16_t *input,
    size_t input_voxels,
    uint32_t nx,
    uint32_t ny,
    uint32_t nz,
    const radelta_lossy_options *options,
    uint8_t *output,
    size_t output_capacity,
    size_t *output_size
);

/*
 * Decompress either lossless RDL1 or lossy RDLQ data. Call with output=NULL
 * to query dimensions and required output voxels. Lossless decoding writes
 * directly into a sufficiently large caller-owned output buffer.
 */
int32_t radelta_decompress_u16(
    const uint8_t *input,
    size_t input_size,
    uint16_t *output,
    size_t output_capacity_voxels,
    size_t *output_voxels,
    uint32_t *nx,
    uint32_t *ny,
    uint32_t *nz
);

/*
 * Multidimensional API. Input/output memory is canonical contiguous T,C,Z,Y,X
 * with X fastest. X and Y are independent and may be rectangular. Spatial
 * prediction contexts never cross channel or time boundaries.
 *
 * New multidimensional containers use RDM2 (lossless) and RDQ2 (lossy).
 */
int32_t radelta_compress_u16_nd(
    const uint16_t *input,
    size_t input_voxels,
    uint32_t nx,
    uint32_t ny,
    uint32_t nz,
    uint32_t nc,
    uint32_t nt,
    const radelta_options *options,
    uint8_t *output,
    size_t output_capacity,
    size_t *output_size
);

int32_t radelta_compress_lossy_u16_nd(
    const uint16_t *input,
    size_t input_voxels,
    uint32_t nx,
    uint32_t ny,
    uint32_t nz,
    uint32_t nc,
    uint32_t nt,
    const radelta_lossy_options *options,
    uint8_t *output,
    size_t output_capacity,
    size_t *output_size
);

/* Decode RDL1/RDLQ/RDM2/RDQ2 to canonical contiguous T,C,Z,Y,X. */
int32_t radelta_decompress_u16_nd(
    const uint8_t *input,
    size_t input_size,
    uint16_t *output,
    size_t output_capacity_voxels,
    size_t *output_voxels,
    uint32_t *nx,
    uint32_t *ny,
    uint32_t *nz,
    uint32_t *nc,
    uint32_t *nt
);

/* -------------------------------------------------------------------------
 * File-oriented reader API for applications such as ImageJ/Fiji.
 *
 * Opens RDL1/RDLQ/RDM2/RDQ2/RDS3/RQS3 files and provides random TCZ plane
 * access.  RDS3/RQS3 payloads are indexed at open time and decoded one chunk
 * at a time, so dataset size is not limited by RAM.
 * ------------------------------------------------------------------------- */

typedef struct radelta_file_handle radelta_file_handle;

typedef struct radelta_file_info {
    uint32_t nx;
    uint32_t ny;
    uint32_t nz;
    uint32_t nc;
    uint32_t nt;
    uint32_t format;
    uint32_t flags;
} radelta_file_info;

enum {
    RADELTA_FILE_FORMAT_RDL1 = 1,
    RADELTA_FILE_FORMAT_RDLQ = 2,
    RADELTA_FILE_FORMAT_RDM2 = 3,
    RADELTA_FILE_FORMAT_RDQ2 = 4,
    RADELTA_FILE_FORMAT_RDS3 = 5,
    RADELTA_FILE_FORMAT_RQS3 = 6
};

enum {
    RADELTA_FILE_FLAG_LOSSY = 1,
    RADELTA_FILE_FLAG_MULTIDIMENSIONAL = 2,
    RADELTA_FILE_FLAG_STREAMING = 4
};

int32_t radelta_file_open(const char *path_utf8, radelta_file_handle **out_handle);
void radelta_file_close(radelta_file_handle *handle);
int32_t radelta_file_get_info(const radelta_file_handle *handle, radelta_file_info *out_info);
int32_t radelta_file_read_plane_u16(
    const radelta_file_handle *handle,
    uint32_t t,
    uint32_t c,
    uint32_t z,
    uint16_t *output,
    uint64_t output_capacity_voxels
);

/* -------------------------------------------------------------------------
 * File-oriented writer API used by applications such as ImageJ/Fiji.
 *
 * The writer always emits chunked out-of-core containers (RDS3 or RQS3) and
 * accepts input as sequential TCZ planes. Callers must provide planes in
 * increasing T,C,Z order. Memory use is bounded by the selected RAM budget.
 * ------------------------------------------------------------------------- */

typedef struct radelta_writer_handle radelta_writer_handle;

int32_t radelta_writer_create_lossless_u16(
    const char *path_utf8,
    uint32_t nx,
    uint32_t ny,
    uint32_t nz,
    uint32_t nc,
    uint32_t nt,
    uint32_t memory_mib,
    radelta_writer_handle **out_handle
);

int32_t radelta_writer_create_lossy_u16(
    const char *path_utf8,
    uint32_t nx,
    uint32_t ny,
    uint32_t nz,
    uint32_t nc,
    uint32_t nt,
    double offset_adu,
    double gain_e_per_adu,
    double noise_step,
    uint32_t memory_mib,
    radelta_writer_handle **out_handle
);

int32_t radelta_writer_write_plane_u16(
    radelta_writer_handle *handle,
    uint32_t t,
    uint32_t c,
    uint32_t z,
    const uint16_t *input,
    uint64_t input_voxels
);

/* Process-wide metadata allocation policy, in bytes; default 1024 MiB.
 * Configure before starting I/O. Zero permits only empty metadata.
 * Existing readers retain loaded metadata; writers recheck at finish. */
void radelta_set_metadata_limit(size_t max_bytes);
size_t radelta_get_metadata_limit(void);

/* Opaque metadata, compressed independently with LZ4.
 * NULL output queries size. Zero metadata_size removes metadata.
 * Return codes follow the existing two-call buffer convention. */
int32_t radelta_get_metadata(const uint8_t *input, size_t input_size,
    uint8_t *output, size_t capacity, size_t *output_size);
int32_t radelta_set_metadata(const uint8_t *input, size_t input_size,
    const uint8_t *metadata, size_t metadata_size,
    uint8_t *output, size_t capacity, size_t *output_size);
int32_t radelta_file_get_metadata(radelta_file_handle *handle,
    uint8_t *output, size_t capacity, size_t *output_size);
/* Must be called before finish; data is copied and may be released on return. */
int32_t radelta_writer_set_metadata(radelta_writer_handle *handle,
    const uint8_t *data, size_t size);

int32_t radelta_writer_finish(radelta_writer_handle *handle);
void radelta_writer_close(radelta_writer_handle *handle);

/* Returns required bytes including terminating NUL. */
uint64_t radelta_file_last_error(char *buffer, uint64_t capacity);

#ifdef __cplusplus
}
#endif

#endif
