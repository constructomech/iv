/*
 * Thin dynamic-loading wrapper around libheif.
 *
 * The main executable compiles this wrapper but loads heif.dll at decode time.
 * This keeps the LGPL library replaceable and avoids linking libheif and its
 * codec dependencies into iv.exe.
 */

#include <libheif/heif.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#ifdef _WIN32
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#endif

typedef struct IvHeifApi {
    int loaded;
    char error[256];

#ifdef _WIN32
    HMODULE heif;
#endif

    heif_context *(*heif_context_alloc)(void);
    void (*heif_context_free)(heif_context *ctx);
    heif_error (*heif_context_read_from_file)(heif_context *ctx, const char *filename, const heif_reading_options *options);
    heif_error (*heif_context_read_from_memory_without_copy)(heif_context *ctx, const void *mem, size_t size, const heif_reading_options *options);
    heif_error (*heif_context_get_primary_image_handle)(heif_context *ctx, heif_image_handle **handle);

    void (*heif_image_handle_release)(const heif_image_handle *handle);
    int (*heif_image_handle_get_number_of_thumbnails)(const heif_image_handle *handle);
    int (*heif_image_handle_get_list_of_thumbnail_IDs)(const heif_image_handle *handle, heif_item_id *ids, int count);
    heif_error (*heif_image_handle_get_thumbnail)(const heif_image_handle *main_image_handle, heif_item_id thumbnail_id, heif_image_handle **out_thumbnail_handle);

    heif_decoding_options *(*heif_decoding_options_alloc)(void);
    void (*heif_decoding_options_free)(heif_decoding_options *options);

    heif_error (*heif_decode_image)(const heif_image_handle *in_handle, heif_image **out_img, enum heif_colorspace colorspace, enum heif_chroma chroma, const heif_decoding_options *options);
    heif_error (*heif_context_get_encoder_for_format)(heif_context *context, enum heif_compression_format format, heif_encoder **encoder);
    void (*heif_encoder_release)(heif_encoder *encoder);
    heif_error (*heif_encoder_set_lossy_quality)(heif_encoder *encoder, int quality);
    heif_error (*heif_image_create)(int width, int height, enum heif_colorspace colorspace, enum heif_chroma chroma, heif_image **out_image);
    heif_error (*heif_image_add_plane)(heif_image *image, enum heif_channel channel, int width, int height, int bit_depth);
    uint8_t *(*heif_image_get_plane)(heif_image *image, enum heif_channel channel, int *out_stride);
    heif_error (*heif_context_encode_image)(heif_context *context, const heif_image *image, heif_encoder *encoder, const heif_encoding_options *options, heif_image_handle **out_image_handle);
    heif_error (*heif_context_encode_thumbnail)(heif_context *context, const heif_image *image, const heif_image_handle *master_image_handle, heif_encoder *encoder, const heif_encoding_options *options, int bbox_size, heif_image_handle **out_thumb_image_handle);
    heif_error (*heif_context_write_to_file)(heif_context *context, const char *filename);
    int (*heif_image_get_width)(const heif_image *img, enum heif_channel channel);
    int (*heif_image_get_height)(const heif_image *img, enum heif_channel channel);
    const uint8_t *(*heif_image_get_plane_readonly)(const heif_image *img, enum heif_channel channel, int *out_stride);
    void (*heif_image_release)(const heif_image *image);
} IvHeifApi;

static IvHeifApi g_heif = {0};

#ifdef _WIN32
static INIT_ONCE g_heif_once = INIT_ONCE_STATIC_INIT;

static void iv_heif_set_error(const char *message) {
    snprintf(g_heif.error, sizeof(g_heif.error), "%s", message);
}

static FARPROC iv_heif_get_proc(HMODULE module, const char *name) {
    FARPROC proc = GetProcAddress(module, name);
    if (!proc) {
        snprintf(g_heif.error, sizeof(g_heif.error), "missing libheif symbol: %s", name);
    }
    return proc;
}

static HMODULE iv_heif_load_library(const char *name) {
    HMODULE module = LoadLibraryA(name);
    if (module) return module;

    char path[MAX_PATH];
    DWORD len = GetFullPathNameA("target\\vcpkg\\installed\\x64-windows\\bin", MAX_PATH, path, NULL);
    if (len == 0 || len >= MAX_PATH) return NULL;

    SetDllDirectoryA(path);

    size_t used = strlen(path);
    if (used + 1 + strlen(name) + 1 >= sizeof(path)) return NULL;
    path[used] = '\\';
    snprintf(path + used + 1, sizeof(path) - used - 1, "%s", name);

    return LoadLibraryExA(path, NULL, LOAD_WITH_ALTERED_SEARCH_PATH);
}

#define IV_HEIF_LOAD_SYMBOL(field) \
    do { \
        g_heif.field = (void *)iv_heif_get_proc(g_heif.heif, #field); \
        if (!g_heif.field) return -1; \
    } while (0)

static int iv_heif_load_inner(void) {
    g_heif.heif = iv_heif_load_library("heif.dll");
    if (!g_heif.heif) {
        iv_heif_set_error("failed to load libheif runtime DLLs");
        return -1;
    }

    IV_HEIF_LOAD_SYMBOL(heif_context_alloc);
    IV_HEIF_LOAD_SYMBOL(heif_context_free);
    IV_HEIF_LOAD_SYMBOL(heif_context_read_from_file);
    IV_HEIF_LOAD_SYMBOL(heif_context_read_from_memory_without_copy);
    IV_HEIF_LOAD_SYMBOL(heif_context_get_primary_image_handle);
    IV_HEIF_LOAD_SYMBOL(heif_image_handle_release);
    IV_HEIF_LOAD_SYMBOL(heif_image_handle_get_number_of_thumbnails);
    IV_HEIF_LOAD_SYMBOL(heif_image_handle_get_list_of_thumbnail_IDs);
    IV_HEIF_LOAD_SYMBOL(heif_image_handle_get_thumbnail);
    IV_HEIF_LOAD_SYMBOL(heif_decoding_options_alloc);
    IV_HEIF_LOAD_SYMBOL(heif_decoding_options_free);
    IV_HEIF_LOAD_SYMBOL(heif_decode_image);
    IV_HEIF_LOAD_SYMBOL(heif_context_get_encoder_for_format);
    IV_HEIF_LOAD_SYMBOL(heif_encoder_release);
    IV_HEIF_LOAD_SYMBOL(heif_encoder_set_lossy_quality);
    IV_HEIF_LOAD_SYMBOL(heif_image_create);
    IV_HEIF_LOAD_SYMBOL(heif_image_add_plane);
    IV_HEIF_LOAD_SYMBOL(heif_image_get_plane);
    IV_HEIF_LOAD_SYMBOL(heif_context_encode_image);
    IV_HEIF_LOAD_SYMBOL(heif_context_encode_thumbnail);
    IV_HEIF_LOAD_SYMBOL(heif_context_write_to_file);
    IV_HEIF_LOAD_SYMBOL(heif_image_get_width);
    IV_HEIF_LOAD_SYMBOL(heif_image_get_height);
    IV_HEIF_LOAD_SYMBOL(heif_image_get_plane_readonly);
    IV_HEIF_LOAD_SYMBOL(heif_image_release);

    g_heif.loaded = 1;
    return 0;
}

static BOOL CALLBACK iv_heif_load_once(PINIT_ONCE init_once, PVOID parameter, PVOID *context) {
    (void)init_once;
    (void)parameter;
    (void)context;
    return iv_heif_load_inner() == 0;
}

static int iv_heif_load(void) {
    if (g_heif.loaded) return 0;
    if (!InitOnceExecuteOnce(&g_heif_once, iv_heif_load_once, NULL, NULL)) {
        if (g_heif.error[0] == '\0') {
            iv_heif_set_error("failed to initialize libheif DLL loader");
        }
        return -1;
    }
    return 0;
}
#else
static int iv_heif_load(void) {
    snprintf(g_heif.error, sizeof(g_heif.error), "dynamic libheif decoding is implemented only for Windows");
    return -1;
}
#endif

static int iv_heif_fail(heif_error err, char *out, int out_len) {
    if (out && out_len > 0) {
        snprintf(out, (size_t)out_len, "%s", err.message ? err.message : "libheif decode failed");
    }
    return err.code ? (int)err.code : -1;
}

static int iv_heif_copy_rgba(const heif_image *image, unsigned char **out_data, int *out_width, int *out_height, char *err, int err_len) {
    int width = g_heif.heif_image_get_width(image, heif_channel_interleaved);
    int height = g_heif.heif_image_get_height(image, heif_channel_interleaved);
    int stride = 0;
    const uint8_t *data = g_heif.heif_image_get_plane_readonly(image, heif_channel_interleaved, &stride);
    if (!data || width <= 0 || height <= 0 || stride < width * 4) {
        if (err && err_len > 0) snprintf(err, (size_t)err_len, "libheif returned invalid RGBA pixels");
        return -1;
    }

    size_t row_bytes = (size_t)width * 4;
    size_t len = row_bytes * (size_t)height;
    unsigned char *pixels = (unsigned char *)malloc(len);
    if (!pixels) {
        if (err && err_len > 0) snprintf(err, (size_t)err_len, "failed to allocate HEIC pixel buffer");
        return -1;
    }

    for (int y = 0; y < height; y++) {
        memcpy(pixels + (size_t)y * row_bytes, data + (size_t)y * (size_t)stride, row_bytes);
    }

    *out_data = pixels;
    *out_width = width;
    *out_height = height;
    return 0;
}

static int iv_heif_decode_handle(const heif_image_handle *handle, unsigned char **out_data, int *out_width, int *out_height, char *err, int err_len) {
    heif_image *image = NULL;

    /* Prefer the FFmpeg HEVC decoder plugin if our libheif build includes
     * it (vcpkg `ffmpeg-decoder` feature). FFmpeg's HEVC decoder is much
     * more SIMD-optimized than libheif's default libde265 backend. The
     * upstream FFmpeg plugin registers itself with priority 90 vs
     * libde265's 100, so without explicit selection libde265 always wins.
     * We override that here.
     *
     * If the running heif.dll doesn't have the FFmpeg plugin compiled in
     * (e.g., the user dropped in a different build), libheif returns
     * "Plugin loading error" and we retry without the decoder hint to fall
     * back to whatever decoder is registered (libde265, AV1, etc.).
     */
    heif_decoding_options *options = g_heif.heif_decoding_options_alloc();
    if (options) {
        options->decoder_id = "ffmpeg";
    }
    heif_error heif_err = g_heif.heif_decode_image(handle, &image, heif_colorspace_RGB, heif_chroma_interleaved_RGBA, options);
    if (heif_err.code != heif_error_Ok && options) {
        /* FFmpeg plugin unavailable in this libheif build — retry with the
         * default decoder. We free the previous options first to avoid a
         * leak, even though heif_decode_image is supposed to leave them
         * untouched on failure. */
        g_heif.heif_decoding_options_free(options);
        options = NULL;
        if (image) {
            g_heif.heif_image_release(image);
            image = NULL;
        }
        heif_err = g_heif.heif_decode_image(handle, &image, heif_colorspace_RGB, heif_chroma_interleaved_RGBA, NULL);
    }
    if (options) g_heif.heif_decoding_options_free(options);
    if (heif_err.code != heif_error_Ok) return iv_heif_fail(heif_err, err, err_len);
    if (!image) {
        if (err && err_len > 0) snprintf(err, (size_t)err_len, "libheif did not return an image");
        return -1;
    }

    int ret = iv_heif_copy_rgba(image, out_data, out_width, out_height, err, err_len);
    g_heif.heif_image_release(image);
    return ret;
}

static int iv_heif_decode_context(heif_context *ctx, int thumbnail_only, unsigned char **out_data, int *out_width, int *out_height, char *err, int err_len) {
    heif_image_handle *primary = NULL;
    heif_image_handle *target = NULL;
    heif_error heif_err = g_heif.heif_context_get_primary_image_handle(ctx, &primary);
    if (heif_err.code != heif_error_Ok) return iv_heif_fail(heif_err, err, err_len);
    if (!primary) {
        if (err && err_len > 0) snprintf(err, (size_t)err_len, "HEIC file has no primary image");
        return -1;
    }

    if (thumbnail_only) {
        int count = g_heif.heif_image_handle_get_number_of_thumbnails(primary);
        if (count <= 0) {
            g_heif.heif_image_handle_release(primary);
            if (err && err_len > 0) snprintf(err, (size_t)err_len, "HEIC file has no thumbnail");
            return -1;
        }
        heif_item_id thumb_id = 0;
        if (g_heif.heif_image_handle_get_list_of_thumbnail_IDs(primary, &thumb_id, 1) <= 0) {
            g_heif.heif_image_handle_release(primary);
            if (err && err_len > 0) snprintf(err, (size_t)err_len, "failed to get HEIC thumbnail id");
            return -1;
        }
        heif_err = g_heif.heif_image_handle_get_thumbnail(primary, thumb_id, &target);
        if (heif_err.code != heif_error_Ok) {
            g_heif.heif_image_handle_release(primary);
            return iv_heif_fail(heif_err, err, err_len);
        }
    } else {
        target = primary;
        primary = NULL;
    }

    int ret = iv_heif_decode_handle(target, out_data, out_width, out_height, err, err_len);
    if (target) g_heif.heif_image_handle_release(target);
    if (primary) g_heif.heif_image_handle_release(primary);
    return ret;
}

int iv_heif_decode_file(const char *path, int thumbnail_only, unsigned char **out_data, int *out_width, int *out_height, char *err, int err_len) {
    if (err && err_len > 0) err[0] = '\0';
    if (!path || !out_data || !out_width || !out_height) return -1;
    *out_data = NULL;
    *out_width = 0;
    *out_height = 0;

    if (iv_heif_load() != 0) {
        if (err && err_len > 0) snprintf(err, (size_t)err_len, "%s", g_heif.error);
        return -1;
    }

    heif_context *ctx = g_heif.heif_context_alloc();
    if (!ctx) {
        if (err && err_len > 0) snprintf(err, (size_t)err_len, "failed to allocate libheif context");
        return -1;
    }

    heif_error heif_err = g_heif.heif_context_read_from_file(ctx, path, NULL);
    if (heif_err.code != heif_error_Ok) {
        g_heif.heif_context_free(ctx);
        return iv_heif_fail(heif_err, err, err_len);
    }

    int ret = iv_heif_decode_context(ctx, thumbnail_only, out_data, out_width, out_height, err, err_len);
    g_heif.heif_context_free(ctx);
    return ret;
}

int iv_heif_decode_memory(const void *data, size_t len, int thumbnail_only, unsigned char **out_data, int *out_width, int *out_height, char *err, int err_len) {
    if (err && err_len > 0) err[0] = '\0';
    if (!data || len == 0 || !out_data || !out_width || !out_height) return -1;
    *out_data = NULL;
    *out_width = 0;
    *out_height = 0;

    if (iv_heif_load() != 0) {
        if (err && err_len > 0) snprintf(err, (size_t)err_len, "%s", g_heif.error);
        return -1;
    }

    heif_context *ctx = g_heif.heif_context_alloc();
    if (!ctx) {
        if (err && err_len > 0) snprintf(err, (size_t)err_len, "failed to allocate libheif context");
        return -1;
    }

    heif_error heif_err = g_heif.heif_context_read_from_memory_without_copy(ctx, data, len, NULL);
    if (heif_err.code != heif_error_Ok) {
        g_heif.heif_context_free(ctx);
        return iv_heif_fail(heif_err, err, err_len);
    }

    int ret = iv_heif_decode_context(ctx, thumbnail_only, out_data, out_width, out_height, err, err_len);
    g_heif.heif_context_free(ctx);
    return ret;
}

static int iv_heif_copy_rgb_to_image(heif_image *image, const unsigned char *rgb, int width, int height, char *err, int err_len) {
    enum heif_channel channels[3] = {heif_channel_R, heif_channel_G, heif_channel_B};
    for (int channel_index = 0; channel_index < 3; channel_index++) {
        int stride = 0;
        uint8_t *dst = g_heif.heif_image_get_plane(image, channels[channel_index], &stride);
        if (!dst || stride < width) {
            if (err && err_len > 0) snprintf(err, (size_t)err_len, "libheif returned invalid encode plane");
            return -1;
        }

        for (int y = 0; y < height; y++) {
            uint8_t *row = dst + (size_t)y * (size_t)stride;
            const unsigned char *src = rgb + (size_t)y * (size_t)width * 3;
            for (int x = 0; x < width; x++) {
                row[x] = src[x * 3 + channel_index];
            }
        }
    }
    return 0;
}

int iv_heif_encode_av1_rgb_file(const char *path, const unsigned char *rgb, int width, int height, int quality, int thumb_quality, int thumb_size, char *err, int err_len) {
    if (err && err_len > 0) err[0] = '\0';
    if (!path || !rgb || width <= 0 || height <= 0) return -1;

    if (iv_heif_load() != 0) {
        if (err && err_len > 0) snprintf(err, (size_t)err_len, "%s", g_heif.error);
        return -1;
    }

    heif_context *ctx = g_heif.heif_context_alloc();
    if (!ctx) {
        if (err && err_len > 0) snprintf(err, (size_t)err_len, "failed to allocate libheif context");
        return -1;
    }

    heif_image *image = NULL;
    heif_image_handle *handle = NULL;
    heif_image_handle *thumb_handle = NULL;
    heif_encoder *encoder = NULL;
    heif_encoder *thumb_encoder = NULL;
    int ret = -1;

    heif_error heif_err = g_heif.heif_image_create(width, height, heif_colorspace_RGB, heif_chroma_444, &image);
    if (heif_err.code != heif_error_Ok) goto fail;

    heif_err = g_heif.heif_image_add_plane(image, heif_channel_R, width, height, 8);
    if (heif_err.code != heif_error_Ok) goto fail;
    heif_err = g_heif.heif_image_add_plane(image, heif_channel_G, width, height, 8);
    if (heif_err.code != heif_error_Ok) goto fail;
    heif_err = g_heif.heif_image_add_plane(image, heif_channel_B, width, height, 8);
    if (heif_err.code != heif_error_Ok) goto fail;

    if (iv_heif_copy_rgb_to_image(image, rgb, width, height, err, err_len) != 0) goto cleanup;

    heif_err = g_heif.heif_context_get_encoder_for_format(ctx, heif_compression_AV1, &encoder);
    if (heif_err.code != heif_error_Ok) goto fail;
    heif_err = g_heif.heif_encoder_set_lossy_quality(encoder, quality);
    if (heif_err.code != heif_error_Ok) goto fail;
    heif_err = g_heif.heif_context_encode_image(ctx, image, encoder, NULL, &handle);
    if (heif_err.code != heif_error_Ok) goto fail;

    heif_err = g_heif.heif_context_get_encoder_for_format(ctx, heif_compression_AV1, &thumb_encoder);
    if (heif_err.code != heif_error_Ok) goto fail;
    heif_err = g_heif.heif_encoder_set_lossy_quality(thumb_encoder, thumb_quality);
    if (heif_err.code != heif_error_Ok) goto fail;
    heif_err = g_heif.heif_context_encode_thumbnail(ctx, image, handle, thumb_encoder, NULL, thumb_size, &thumb_handle);
    if (heif_err.code != heif_error_Ok) goto fail;

    heif_err = g_heif.heif_context_write_to_file(ctx, path);
    if (heif_err.code != heif_error_Ok) goto fail;

    ret = 0;
    goto cleanup;

fail:
    iv_heif_fail(heif_err, err, err_len);

cleanup:
    if (thumb_handle) g_heif.heif_image_handle_release(thumb_handle);
    if (handle) g_heif.heif_image_handle_release(handle);
    if (thumb_encoder) g_heif.heif_encoder_release(thumb_encoder);
    if (encoder) g_heif.heif_encoder_release(encoder);
    if (image) g_heif.heif_image_release(image);
    g_heif.heif_context_free(ctx);
    return ret;
}

void iv_heif_free(void *ptr) {
    free(ptr);
}
