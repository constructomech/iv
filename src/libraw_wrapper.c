/*
 * Thin C wrapper around LibRaw.
 *
 * We wrap the full decode pipeline in a single C function so the Rust side
 * never needs to know the layout of libraw_data_t (which is huge and
 * version-sensitive). Only the small libraw_processed_image_t matters,
 * and even that is hidden behind a malloc'd buffer we hand back.
 */

#include <libraw/libraw.h>
#include <stdlib.h>
#include <string.h>

/*
 * Decode a raw image from an in-memory buffer to an 8-bit RGB bitmap.
 *
 * Parameters:
 *   data, len       – raw file bytes (DNG, CR2, NEF, ARW, …)
 *   out_data        – receives malloc'd RGB pixel buffer (caller frees via iv_libraw_free)
 *   out_width       – image width in pixels
 *   out_height      – image height in pixels
 *   out_colors      – color channels (always 3 for RGB)
 *   out_data_size   – byte length of *out_data
 *
 * Returns 0 on success, non-zero LibRaw error code on failure.
 * LibRaw applies camera white balance and EXIF orientation internally.
 */
int iv_libraw_decode_buffer(
    const void *data, size_t len,
    unsigned char **out_data,
    int *out_width, int *out_height,
    int *out_colors,
    unsigned int *out_data_size)
{
    libraw_data_t *raw = libraw_init(0);
    if (!raw) return -1;

    /* Output settings */
    raw->params.output_bps    = 8;   /* 8 bits per channel              */
    raw->params.output_color  = 1;   /* sRGB colour space               */
    raw->params.use_camera_wb = 1;   /* use camera white balance        */
    raw->params.no_auto_bright = 1;  /* no auto-brightness (consistent) */

    int ret = libraw_open_buffer(raw, (void *)data, len);
    if (ret != 0) { libraw_close(raw); return ret; }

    ret = libraw_unpack(raw);
    if (ret != 0) { libraw_close(raw); return ret; }

    ret = libraw_dcraw_process(raw);
    if (ret != 0) { libraw_close(raw); return ret; }

    int errcode = 0;
    libraw_processed_image_t *img = libraw_dcraw_make_mem_image(raw, &errcode);
    if (!img) { libraw_close(raw); return errcode ? errcode : -1; }

    *out_width     = img->width;
    *out_height    = img->height;
    *out_colors    = img->colors;
    *out_data_size = img->data_size;

    *out_data = (unsigned char *)malloc(img->data_size);
    if (*out_data) {
        memcpy(*out_data, img->data, img->data_size);
    } else {
        ret = -1;
    }

    libraw_dcraw_clear_mem(img);
    libraw_close(raw);
    return ret;
}

/* Free a buffer previously returned by iv_libraw_decode_buffer.
 * Must use the same allocator (CRT malloc/free). */
void iv_libraw_free(void *ptr) {
    free(ptr);
}
