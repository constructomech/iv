/*
 * Thin dynamic-loading wrapper around FFmpeg.
 *
 * iv links this object into the main binary, but FFmpeg itself is loaded with
 * LoadLibrary at thumbnail decode time. That keeps startup independent from the
 * LGPL DLLs while still using FFmpeg's real-world video decoder coverage.
 */

#include <libavcodec/avcodec.h>
#include <libavcodec/packet.h>
#include <libavformat/avformat.h>
#include <libavutil/display.h>
#include <libavutil/error.h>
#include <libavutil/imgutils.h>
#include <libavutil/log.h>
#include <libavutil/pixfmt.h>
#include <libswscale/swscale.h>
#include <math.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#ifdef _WIN32
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#endif

#define IV_VIDEO_THUMB_MAX_PACKETS 8

typedef struct IvFfmpegApi {
    int loaded;
    char error[256];

#ifdef _WIN32
    HMODULE avcodec;
    HMODULE avformat;
    HMODULE avutil;
    HMODULE swscale;
#endif

    AVFormatContext *(*avformat_alloc_context)(void);
    void (*avformat_free_context)(AVFormatContext *s);
    int (*avformat_open_input)(AVFormatContext **ps, const char *url, const AVInputFormat *fmt, AVDictionary **options);
    void (*avformat_close_input)(AVFormatContext **s);
    int (*avformat_find_stream_info)(AVFormatContext *ic, AVDictionary **options);
    int (*av_find_best_stream)(AVFormatContext *ic, enum AVMediaType type, int wanted_stream_nb, int related_stream, const AVCodec **decoder_ret, int flags);
    int (*av_read_frame)(AVFormatContext *s, AVPacket *pkt);

    const AVCodec *(*avcodec_find_decoder)(enum AVCodecID id);
    AVCodecContext *(*avcodec_alloc_context3)(const AVCodec *codec);
    int (*avcodec_parameters_to_context)(AVCodecContext *codec, const AVCodecParameters *par);
    int (*avcodec_open2)(AVCodecContext *avctx, const AVCodec *codec, AVDictionary **options);
    int (*avcodec_send_packet)(AVCodecContext *avctx, const AVPacket *avpkt);
    int (*avcodec_receive_frame)(AVCodecContext *avctx, AVFrame *frame);
    void (*avcodec_free_context)(AVCodecContext **avctx);
    const AVPacketSideData *(*av_packet_side_data_get)(const AVPacketSideData *sd, int nb_sd, enum AVPacketSideDataType type);

    AVPacket *(*av_packet_alloc)(void);
    void (*av_packet_free)(AVPacket **pkt);
    void (*av_packet_unref)(AVPacket *pkt);
    AVFrame *(*av_frame_alloc)(void);
    void (*av_frame_free)(AVFrame **frame);
    double (*av_display_rotation_get)(const int32_t matrix[9]);
    int (*av_image_fill_arrays)(uint8_t *dst_data[4], int dst_linesize[4], const uint8_t *src, enum AVPixelFormat pix_fmt, int width, int height, int align);
    void (*av_log_set_level)(int level);
    int (*av_strerror)(int errnum, char *errbuf, size_t errbuf_size);

    struct SwsContext *(*sws_getContext)(int srcW, int srcH, enum AVPixelFormat srcFormat, int dstW, int dstH, enum AVPixelFormat dstFormat, int flags, SwsFilter *srcFilter, SwsFilter *dstFilter, const double *param);
    int (*sws_scale)(struct SwsContext *c, const uint8_t *const srcSlice[], const int srcStride[], int srcSliceY, int srcSliceH, uint8_t *const dst[], const int dstStride[]);
    void (*sws_freeContext)(struct SwsContext *swsContext);
} IvFfmpegApi;

static IvFfmpegApi g_ffmpeg = {0};

#ifdef _WIN32
static INIT_ONCE g_ffmpeg_once = INIT_ONCE_STATIC_INIT;
#endif

#ifdef _WIN32
static void iv_set_error(const char *message) {
    snprintf(g_ffmpeg.error, sizeof(g_ffmpeg.error), "%s", message);
}

static FARPROC iv_get_proc(HMODULE module, const char *name) {
    FARPROC proc = GetProcAddress(module, name);
    if (!proc) {
        snprintf(g_ffmpeg.error, sizeof(g_ffmpeg.error), "missing FFmpeg symbol: %s", name);
    }
    return proc;
}

static HMODULE iv_load_library(const char *name) {
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

#define IV_LOAD_SYMBOL(module, field) \
    do { \
        g_ffmpeg.field = (void *)iv_get_proc(g_ffmpeg.module, #field); \
        if (!g_ffmpeg.field) return -1; \
    } while (0)

static int iv_ffmpeg_load_inner(void) {
    g_ffmpeg.avutil = iv_load_library("avutil-60.dll");
    g_ffmpeg.avcodec = iv_load_library("avcodec-62.dll");
    g_ffmpeg.avformat = iv_load_library("avformat-62.dll");
    g_ffmpeg.swscale = iv_load_library("swscale-9.dll");
    if (!g_ffmpeg.avutil || !g_ffmpeg.avcodec || !g_ffmpeg.avformat || !g_ffmpeg.swscale) {
        iv_set_error("failed to load FFmpeg runtime DLLs");
        return -1;
    }

    IV_LOAD_SYMBOL(avformat, avformat_alloc_context);
    IV_LOAD_SYMBOL(avformat, avformat_free_context);
    IV_LOAD_SYMBOL(avformat, avformat_open_input);
    IV_LOAD_SYMBOL(avformat, avformat_close_input);
    IV_LOAD_SYMBOL(avformat, avformat_find_stream_info);
    IV_LOAD_SYMBOL(avformat, av_find_best_stream);
    IV_LOAD_SYMBOL(avformat, av_read_frame);

    IV_LOAD_SYMBOL(avcodec, avcodec_find_decoder);
    IV_LOAD_SYMBOL(avcodec, avcodec_alloc_context3);
    IV_LOAD_SYMBOL(avcodec, avcodec_parameters_to_context);
    IV_LOAD_SYMBOL(avcodec, avcodec_open2);
    IV_LOAD_SYMBOL(avcodec, avcodec_send_packet);
    IV_LOAD_SYMBOL(avcodec, avcodec_receive_frame);
    IV_LOAD_SYMBOL(avcodec, avcodec_free_context);
    IV_LOAD_SYMBOL(avcodec, av_packet_side_data_get);

    IV_LOAD_SYMBOL(avcodec, av_packet_alloc);
    IV_LOAD_SYMBOL(avcodec, av_packet_free);
    IV_LOAD_SYMBOL(avcodec, av_packet_unref);
    IV_LOAD_SYMBOL(avutil, av_frame_alloc);
    IV_LOAD_SYMBOL(avutil, av_frame_free);
    IV_LOAD_SYMBOL(avutil, av_display_rotation_get);
    IV_LOAD_SYMBOL(avutil, av_image_fill_arrays);
    IV_LOAD_SYMBOL(avutil, av_log_set_level);
    IV_LOAD_SYMBOL(avutil, av_strerror);

    IV_LOAD_SYMBOL(swscale, sws_getContext);
    IV_LOAD_SYMBOL(swscale, sws_scale);
    IV_LOAD_SYMBOL(swscale, sws_freeContext);

    g_ffmpeg.loaded = 1;
    g_ffmpeg.av_log_set_level(AV_LOG_QUIET);
    return 0;
}

static BOOL CALLBACK iv_ffmpeg_load_once(PINIT_ONCE init_once, PVOID parameter, PVOID *context) {
    (void)init_once;
    (void)parameter;
    (void)context;
    return iv_ffmpeg_load_inner() == 0;
}

static int iv_ffmpeg_load(void) {
    if (g_ffmpeg.loaded) return 0;
    if (!InitOnceExecuteOnce(&g_ffmpeg_once, iv_ffmpeg_load_once, NULL, NULL)) {
        if (g_ffmpeg.error[0] == '\0') {
            iv_set_error("failed to initialize FFmpeg DLL loader");
        }
        return -1;
    }
    return 0;
}
#else
static int iv_ffmpeg_load(void) {
    snprintf(g_ffmpeg.error, sizeof(g_ffmpeg.error), "dynamic FFmpeg thumbnail decoding is implemented only for Windows");
    return -1;
}
#endif

static void iv_format_error(int err, char *out, int out_len) {
    if (out_len <= 0) return;
    if (g_ffmpeg.loaded && g_ffmpeg.av_strerror && g_ffmpeg.av_strerror(err, out, (size_t)out_len) == 0) {
        return;
    }
    snprintf(out, (size_t)out_len, "FFmpeg error %d", err);
}

static int iv_frame_brightness_score(const unsigned char *rgba, int width, int height) {
    if (!rgba || width <= 0 || height <= 0) return 0;

    const int step_x = width > 64 ? width / 64 : 1;
    const int step_y = height > 64 ? height / 64 : 1;
    int samples = 0;
    int bright = 0;
    int varied = 0;
    int previous = -1;

    for (int y = 0; y < height; y += step_y) {
        for (int x = 0; x < width; x += step_x) {
            const unsigned char *px = rgba + ((size_t)y * (size_t)width + (size_t)x) * 4;
            int luma = ((int)px[0] * 54 + (int)px[1] * 183 + (int)px[2] * 19) >> 8;
            if (luma > 24) bright++;
            if (previous >= 0 && abs(luma - previous) > 8) varied++;
            previous = luma;
            samples++;
        }
    }

    if (samples == 0) return 0;
    return bright * 100 / samples + varied * 30 / samples;
}

static int iv_normalize_rotation(double rotation) {
    if (isnan(rotation)) return 0;

    int degrees = (int)floor(rotation + (rotation >= 0.0 ? 0.5 : -0.5));
    degrees %= 360;
    if (degrees < 0) degrees += 360;

    if (degrees >= 315 || degrees < 45) return 0;
    if (degrees < 135) return 90;
    if (degrees < 225) return 180;
    return 270;
}

static int iv_stream_rotation_degrees(const AVStream *stream) {
    if (!stream || !stream->codecpar || !stream->codecpar->coded_side_data) return 0;

    const AVPacketSideData *side_data = g_ffmpeg.av_packet_side_data_get(
        stream->codecpar->coded_side_data,
        stream->codecpar->nb_coded_side_data,
        AV_PKT_DATA_DISPLAYMATRIX);
    if (!side_data || side_data->size < sizeof(int32_t) * 9) return 0;

    // FFmpeg reports display-matrix rotation counterclockwise; iv_apply_rotation
    // maps pixels clockwise.
    return iv_normalize_rotation(-g_ffmpeg.av_display_rotation_get((const int32_t *)side_data->data));
}

static int iv_apply_rotation(unsigned char **data, int *width, int *height, int rotation, char *err, int err_len) {
    if (!data || !*data || !width || !height) return -1;

    int src_w = *width;
    int src_h = *height;
    if (rotation == 0) return 0;
    if (src_w <= 0 || src_h <= 0) return -1;

    int dst_w = (rotation == 90 || rotation == 270) ? src_h : src_w;
    int dst_h = (rotation == 90 || rotation == 270) ? src_w : src_h;
    size_t len = (size_t)dst_w * (size_t)dst_h * 4;
    unsigned char *rotated = (unsigned char *)malloc(len);
    if (!rotated) {
        snprintf(err, (size_t)err_len, "failed to allocate rotated video thumbnail buffer");
        return -1;
    }

    const unsigned char *src = *data;
    for (int y = 0; y < src_h; y++) {
        for (int x = 0; x < src_w; x++) {
            int dst_x = x;
            int dst_y = y;

            if (rotation == 90) {
                dst_x = src_h - 1 - y;
                dst_y = x;
            } else if (rotation == 180) {
                dst_x = src_w - 1 - x;
                dst_y = src_h - 1 - y;
            } else if (rotation == 270) {
                dst_x = y;
                dst_y = src_w - 1 - x;
            }

            memcpy(
                rotated + ((size_t)dst_y * (size_t)dst_w + (size_t)dst_x) * 4,
                src + ((size_t)y * (size_t)src_w + (size_t)x) * 4,
                4);
        }
    }

    free(*data);
    *data = rotated;
    *width = dst_w;
    *height = dst_h;
    return 0;
}

static int iv_scale_frame_to_rgba(const AVFrame *frame, int max_size, unsigned char **out_data, int *out_width, int *out_height, char *err, int err_len) {
    if (!frame || !out_data || !out_width || !out_height || max_size <= 0) return -1;
    if (frame->width <= 0 || frame->height <= 0) {
        snprintf(err, (size_t)err_len, "decoded video frame has invalid dimensions");
        return -1;
    }

    double scale = (double)max_size / (double)(frame->width > frame->height ? frame->width : frame->height);
    if (scale > 1.0) scale = 1.0;
    int dst_w = (int)floor((double)frame->width * scale + 0.5);
    int dst_h = (int)floor((double)frame->height * scale + 0.5);
    if (dst_w < 1) dst_w = 1;
    if (dst_h < 1) dst_h = 1;

    size_t len = (size_t)dst_w * (size_t)dst_h * 4;
    unsigned char *rgba = (unsigned char *)malloc(len);
    if (!rgba) {
        snprintf(err, (size_t)err_len, "failed to allocate video thumbnail buffer");
        return -1;
    }

    uint8_t *dst_data[4] = {0};
    int dst_linesize[4] = {0};
    int ret = g_ffmpeg.av_image_fill_arrays(dst_data, dst_linesize, rgba, AV_PIX_FMT_RGBA, dst_w, dst_h, 1);
    if (ret < 0) {
        iv_format_error(ret, err, err_len);
        free(rgba);
        return ret;
    }

    struct SwsContext *sws = g_ffmpeg.sws_getContext(frame->width, frame->height, (enum AVPixelFormat)frame->format, dst_w, dst_h, AV_PIX_FMT_RGBA, SWS_BICUBIC, NULL, NULL, NULL);
    if (!sws) {
        snprintf(err, (size_t)err_len, "failed to create FFmpeg scaling context");
        free(rgba);
        return -1;
    }

    ret = g_ffmpeg.sws_scale(sws, (const uint8_t *const *)frame->data, frame->linesize, 0, frame->height, dst_data, dst_linesize);
    g_ffmpeg.sws_freeContext(sws);
    if (ret <= 0) {
        snprintf(err, (size_t)err_len, "failed to scale decoded video frame");
        free(rgba);
        return -1;
    }

    *out_data = rgba;
    *out_width = dst_w;
    *out_height = dst_h;
    return 0;
}

static int iv_try_receive_frame(AVCodecContext *codec_ctx, AVFrame *frame, int max_size, int rotation, unsigned char **best_data, int *best_width, int *best_height, int *best_score, char *err, int err_len) {
    int ret;
    int received = 0;

    while ((ret = g_ffmpeg.avcodec_receive_frame(codec_ctx, frame)) == 0) {
        received = 1;

        unsigned char *rgba = NULL;
        int width = 0;
        int height = 0;
        ret = iv_scale_frame_to_rgba(frame, max_size, &rgba, &width, &height, err, err_len);
        if (ret < 0) return ret;

        ret = iv_apply_rotation(&rgba, &width, &height, rotation, err, err_len);
        if (ret < 0) return ret;

        int score = iv_frame_brightness_score(rgba, width, height);
        if (score > *best_score) {
            free(*best_data);
            *best_data = rgba;
            *best_width = width;
            *best_height = height;
            *best_score = score;
        } else {
            free(rgba);
        }

        if (*best_score >= 35) return 1;
    }

    if (ret == AVERROR(EAGAIN) || ret == AVERROR_EOF) return received;
    iv_format_error(ret, err, err_len);
    return ret;
}

int iv_ffmpeg_decode_thumbnail(const char *path, int max_size, unsigned char **out_data, int *out_width, int *out_height, char *err, int err_len) {
    if (err && err_len > 0) err[0] = '\0';
    if (!path || !out_data || !out_width || !out_height || max_size <= 0) {
        if (err && err_len > 0) snprintf(err, (size_t)err_len, "invalid video thumbnail arguments");
        return -1;
    }

    *out_data = NULL;
    *out_width = 0;
    *out_height = 0;

    if (iv_ffmpeg_load() != 0) {
        if (err && err_len > 0) snprintf(err, (size_t)err_len, "%s", g_ffmpeg.error);
        return -1;
    }

    AVFormatContext *fmt_ctx = NULL;
    AVCodecContext *codec_ctx = NULL;
    AVPacket *packet = NULL;
    AVFrame *frame = NULL;
    unsigned char *best_data = NULL;
    int best_width = 0;
    int best_height = 0;
    int best_score = -1;
    int ret = 0;

    fmt_ctx = g_ffmpeg.avformat_alloc_context();
    if (!fmt_ctx) {
        snprintf(err, (size_t)err_len, "failed to allocate FFmpeg format context");
        ret = -1;
        goto cleanup;
    }
    fmt_ctx->probesize = 32768;
    fmt_ctx->max_analyze_duration = 0;

    ret = g_ffmpeg.avformat_open_input(&fmt_ctx, path, NULL, NULL);
    if (ret < 0) goto fail_ffmpeg;

    ret = g_ffmpeg.avformat_find_stream_info(fmt_ctx, NULL);
    if (ret < 0) goto fail_ffmpeg;

    const AVCodec *decoder = NULL;
    int stream_index = g_ffmpeg.av_find_best_stream(fmt_ctx, AVMEDIA_TYPE_VIDEO, -1, -1, &decoder, 0);
    if (stream_index < 0) {
        ret = stream_index;
        goto fail_ffmpeg;
    }

    AVStream *stream = fmt_ctx->streams[stream_index];
    int rotation = iv_stream_rotation_degrees(stream);
    if (!decoder) decoder = g_ffmpeg.avcodec_find_decoder(stream->codecpar->codec_id);
    if (!decoder) {
        snprintf(err, (size_t)err_len, "no FFmpeg decoder found for video stream");
        ret = -1;
        goto cleanup;
    }

    codec_ctx = g_ffmpeg.avcodec_alloc_context3(decoder);
    if (!codec_ctx) {
        snprintf(err, (size_t)err_len, "failed to allocate FFmpeg codec context");
        ret = -1;
        goto cleanup;
    }

    ret = g_ffmpeg.avcodec_parameters_to_context(codec_ctx, stream->codecpar);
    if (ret < 0) goto fail_ffmpeg;

    ret = g_ffmpeg.avcodec_open2(codec_ctx, decoder, NULL);
    if (ret < 0) goto fail_ffmpeg;

    packet = g_ffmpeg.av_packet_alloc();
    frame = g_ffmpeg.av_frame_alloc();
    if (!packet || !frame) {
        snprintf(err, (size_t)err_len, "failed to allocate FFmpeg decode buffers");
        ret = -1;
        goto cleanup;
    }

    int packets = 0;
    while (packets < IV_VIDEO_THUMB_MAX_PACKETS && (ret = g_ffmpeg.av_read_frame(fmt_ctx, packet)) >= 0) {
        if (packet->stream_index == stream_index) {
            packets++;
            ret = g_ffmpeg.avcodec_send_packet(codec_ctx, packet);
            g_ffmpeg.av_packet_unref(packet);
            if (ret == AVERROR(EAGAIN)) {
                ret = 0;
            } else if (ret < 0) {
                goto fail_ffmpeg;
            }

            ret = iv_try_receive_frame(codec_ctx, frame, max_size, rotation, &best_data, &best_width, &best_height, &best_score, err, err_len);
            if (ret < 0) goto cleanup;
            if (ret == 1 && best_score >= 35) break;
        } else {
            g_ffmpeg.av_packet_unref(packet);
        }
    }

    ret = g_ffmpeg.avcodec_send_packet(codec_ctx, NULL);
    if (ret >= 0) {
        ret = iv_try_receive_frame(codec_ctx, frame, max_size, rotation, &best_data, &best_width, &best_height, &best_score, err, err_len);
        if (ret < 0) goto cleanup;
    }

    if (!best_data) {
        snprintf(err, (size_t)err_len, "FFmpeg did not decode a video frame");
        ret = -1;
        goto cleanup;
    }

    *out_data = best_data;
    *out_width = best_width;
    *out_height = best_height;
    best_data = NULL;
    ret = 0;
    goto cleanup;

fail_ffmpeg:
    iv_format_error(ret, err, err_len);

cleanup:
    free(best_data);
    if (packet) g_ffmpeg.av_packet_free(&packet);
    if (frame) g_ffmpeg.av_frame_free(&frame);
    if (codec_ctx) g_ffmpeg.avcodec_free_context(&codec_ctx);
    if (fmt_ctx) g_ffmpeg.avformat_close_input(&fmt_ctx);
    return ret == 0 ? 0 : ret;
}

void iv_ffmpeg_free(void *ptr) {
    free(ptr);
}
