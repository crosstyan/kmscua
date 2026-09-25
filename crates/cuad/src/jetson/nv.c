// Jetson NvBufSurface / VIC / NVENC shim for jetson/mod.rs.
//
// The Tegra NVENC is driven through NVIDIA's libv4l2 plugin (libnvv4l2): /dev/v4l2-nvenc is only
// a placeholder node, every v4l2_* call is served in user space by the NvMM stack. The KMS
// scanout is imported as an NvBufSurface, converted and scaled to NV12 on the VIC, and the NV12
// surface's dma-buf is queued to the encoder with V4L2_MEMORY_DMABUF. The only CPU work per
// frame is the cursor blend, a few thousand pixels.
//
// Adapted from the RustDesk scrap Jetson shim (libs/scrap/src/common/jetson_nv.c).

#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

#include <libv4l2.h>
#include <linux/videodev2.h>

#include "nvbufsurface.h"
#include "nvbufsurftransform.h"
#include "v4l2_nv_extensions.h"

#define JZ_MAX_OUT 8
#define JZ_NCAP 4
#define JZ_CAP_SIZE (4 << 20)

enum { JZ_FMT_BGRA = 1, JZ_FMT_BGRX = 2, JZ_FMT_RGBA = 3, JZ_FMT_RGBX = 4 };

static void set_err(char *err, size_t len, const char *what) {
    if (err && len) snprintf(err, len, "%s: %s", what, strerror(errno));
}

static NvBufSurfaceColorFormat color_format(int fmt) {
    switch (fmt) {
    case JZ_FMT_BGRA: return NVBUF_COLOR_FORMAT_BGRA;
    case JZ_FMT_BGRX: return NVBUF_COLOR_FORMAT_BGRx;
    case JZ_FMT_RGBA: return NVBUF_COLOR_FORMAT_RGBA;
    default: return NVBUF_COLOR_FORMAT_RGBx;
    }
}

// Imports a single-plane 32-bit RGB dma-buf (a KMS scanout). `block_height_log2` < 0 means pitch
// linear, otherwise NVIDIA block linear with that GOB block height. The caller keeps `fd` open
// for the surface's lifetime.
NvBufSurface *jz_import(int fd, uint32_t w, uint32_t h, int fmt, uint32_t pitch,
                        int block_height_log2) {
    off_t size = lseek(fd, 0, SEEK_END);
    if (size <= 0) return NULL;
    NvBufSurfaceMapParams mp;
    memset(&mp, 0, sizeof mp);
    mp.num_planes = 1;
    mp.fd = fd;
    mp.totalSize = size;
    mp.memType = NVBUF_MEM_SURFACE_ARRAY;
    mp.layout = block_height_log2 < 0 ? NVBUF_LAYOUT_PITCH : NVBUF_LAYOUT_BLOCK_LINEAR;
    mp.colorFormat = color_format(fmt);
    mp.planes[0].width = w;
    mp.planes[0].height = h;
    mp.planes[0].pitch = pitch;
    mp.planes[0].psize = size;
    mp.planes[0].blockheightlog2 = block_height_log2 < 0 ? 0 : block_height_log2;
    NvBufSurface *s = NULL;
    if (NvBufSurfaceImport(&s, &mp) != 0) return NULL;
    // NvBufSurfTransform rejects surfaces with numFilled == 0 (error -3).
    s->numFilled = 1;
    return s;
}

// Pitch-linear NV12 surface. jz_blend_cursor maps it on first use.
NvBufSurface *jz_alloc_nv12(uint32_t w, uint32_t h) {
    NvBufSurfaceCreateParams cp;
    memset(&cp, 0, sizeof cp);
    cp.width = w;
    cp.height = h;
    cp.layout = NVBUF_LAYOUT_PITCH;
    cp.memType = NVBUF_MEM_SURFACE_ARRAY;
    cp.colorFormat = NVBUF_COLOR_FORMAT_NV12;
    NvBufSurface *s = NULL;
    if (NvBufSurfaceCreate(&s, 1, &cp) != 0) return NULL;
    s->numFilled = 1;
    return s;
}

void jz_destroy(NvBufSurface *s) {
    if (!s) return;
    if (s->surfaceList[0].mappedAddr.addr[0]) NvBufSurfaceUnMap(s, 0, -1);
    NvBufSurfaceDestroy(s);
}

// VIC color conversion plus scaling: the (0,0,src_w,src_h) rectangle of `src` onto all of `dst`.
// Session params are per thread.
int jz_convert(NvBufSurface *src, NvBufSurface *dst, uint32_t src_w, uint32_t src_h) {
    static __thread int session_set;
    if (!session_set) {
        NvBufSurfTransformConfigParams cfg;
        memset(&cfg, 0, sizeof cfg);
        cfg.compute_mode = NvBufSurfTransformCompute_VIC;
        if (NvBufSurfTransformSetSessionParams(&cfg) != NvBufSurfTransformError_Success) return -1;
        session_set = 1;
    }
    NvBufSurfTransformRect sr = {0, 0, src_w, src_h};
    NvBufSurfTransformParams tp;
    memset(&tp, 0, sizeof tp);
    tp.transform_flag = NVBUFSURF_TRANSFORM_CROP_SRC | NVBUFSURF_TRANSFORM_FILTER;
    tp.transform_filter = NvBufSurfTransformInter_Algo4;  // VIC "nicest"
    tp.src_rect = &sr;
    return NvBufSurfTransform(src, dst, &tp) == NvBufSurfTransformError_Success ? 0 : -1;
}

// BT.601 limited range, the matrix the VIC uses for RGB -> NV12 (and what the encoder's VUI says).
static inline int yc(int r, int g, int b) { return (66 * r + 129 * g + 25 * b + 128) >> 8; }
static inline int uc(int r, int g, int b) { return (-38 * r - 74 * g + 112 * b + 128) >> 8; }
static inline int vc(int r, int g, int b) { return (112 * r - 94 * g - 18 * b + 128) >> 8; }
static inline uint8_t clamp8(int v) { return v < 0 ? 0 : v > 255 ? 255 : (uint8_t)v; }

// Box-averages the premultiplied ARGB cursor over the `factor` x `factor` source block of
// destination pixel (dx, dy). Pixels outside the cursor count as transparent.
static void cursor_sample(const uint32_t *px, int cw, int ch, int cx, int cy, int factor, int dx,
                          int dy, int *r, int *g, int *b, int *a) {
    int sr = 0, sg = 0, sb = 0, sa = 0;
    for (int j = 0; j < factor; j++) {
        int y = dy * factor + j - cy;
        if (y < 0 || y >= ch) continue;
        for (int i = 0; i < factor; i++) {
            int x = dx * factor + i - cx;
            if (x < 0 || x >= cw) continue;
            uint32_t p = px[y * cw + x];
            sa += (p >> 24) & 0xff;
            sr += (p >> 16) & 0xff;
            sg += (p >> 8) & 0xff;
            sb += p & 0xff;
        }
    }
    int n = factor * factor;
    *r = sr / n;
    *g = sg / n;
    *b = sb / n;
    *a = sa / n;
}

// Source-over blend of a premultiplied ARGB8888 cursor, placed at (cx, cy) in scanout pixels,
// into an NV12 surface that holds the scanout downscaled by `factor`. YUV is affine in RGB, so
// out = K*c + (1-a)*(dst - offset) + offset per channel; chroma uses the 2x2 average.
int jz_blend_cursor(NvBufSurface *s, const uint32_t *px, int cw, int ch, int cx, int cy,
                    int factor) {
    NvBufSurfaceParams *p = &s->surfaceList[0];
    if (!p->mappedAddr.addr[0] && NvBufSurfaceMap(s, 0, -1, NVBUF_MAP_READ_WRITE) != 0) return -1;
    uint8_t *yp = p->mappedAddr.addr[0], *uvp = p->mappedAddr.addr[1];
    if (!yp || !uvp || factor < 1) return -1;
    int w = (int)p->width, h = (int)p->height;
    int ypitch = (int)p->planeParams.pitch[0], uvpitch = (int)p->planeParams.pitch[1];
    // Destination box covering the cursor, widened to even coordinates for the chroma pairs.
    int x0 = cx >= 0 ? cx / factor : -((-cx + factor - 1) / factor);
    int y0 = cy >= 0 ? cy / factor : -((-cy + factor - 1) / factor);
    int x1 = (cx + cw + factor - 1) / factor, y1 = (cy + ch + factor - 1) / factor;
    x0 = (x0 < 0 ? 0 : x0) & ~1;
    y0 = (y0 < 0 ? 0 : y0) & ~1;
    x1 = x1 > w ? w : x1;
    y1 = y1 > h ? h : y1;
    if (x0 >= x1 || y0 >= y1) return 0;
    if (NvBufSurfaceSyncForCpu(s, 0, -1) != 0) return -1;
    for (int y = y0; y < y1; y += 2) {
        for (int x = x0; x < x1; x += 2) {
            int ar = 0, ag = 0, ab = 0, aa = 0, cnt = 0;
            for (int j = 0; j < 2 && y + j < h; j++) {
                for (int i = 0; i < 2 && x + i < w; i++) {
                    int r, g, b, a;
                    cursor_sample(px, cw, ch, cx, cy, factor, x + i, y + j, &r, &g, &b, &a);
                    ar += r, ag += g, ab += b, aa += a, cnt++;
                    if (a == 0) continue;
                    uint8_t *yy = yp + (size_t)(y + j) * ypitch + x + i;
                    *yy = clamp8(yc(r, g, b) + ((255 - a) * (*yy - 16) + 127) / 255 + 16);
                }
            }
            if (aa == 0) continue;
            ar /= cnt, ag /= cnt, ab /= cnt, aa /= cnt;
            uint8_t *uv = uvp + (size_t)(y / 2) * uvpitch + x;
            uv[0] = clamp8(uc(ar, ag, ab) + ((255 - aa) * (uv[0] - 128) + 127) / 255 + 128);
            uv[1] = clamp8(vc(ar, ag, ab) + ((255 - aa) * (uv[1] - 128) + 127) / 255 + 128);
        }
    }
    return NvBufSurfaceSyncForDevice(s, 0, -1);
}

typedef struct jz_enc {
    int fd;
    void *cap[JZ_NCAP];
    size_t cap_len[JZ_NCAP];
} jz_enc;

static int ctrl(jz_enc *e, uint32_t id, int32_t value) {
    struct v4l2_ext_control c;
    struct v4l2_ext_controls cs;
    memset(&c, 0, sizeof c);
    memset(&cs, 0, sizeof cs);
    c.id = id;
    c.value = value;
    cs.ctrl_class = V4L2_CTRL_CLASS_MPEG;
    cs.count = 1;
    cs.controls = &c;
    return v4l2_ioctl(e->fd, VIDIOC_S_EXT_CTRLS, &cs);
}

static int cap_buf(jz_enc *e, int index, unsigned long req) {
    struct v4l2_plane pl[1];
    struct v4l2_buffer b;
    memset(&b, 0, sizeof b);
    memset(pl, 0, sizeof pl);
    b.type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE;
    b.memory = V4L2_MEMORY_MMAP;
    b.index = index;
    b.m.planes = pl;
    b.length = 1;
    return v4l2_ioctl(e->fd, req, &b);
}

void jz_enc_close(jz_enc *e) {
    if (!e) return;
    if (e->fd >= 0) {
        int t = V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE;
        v4l2_ioctl(e->fd, VIDIOC_STREAMOFF, &t);
        t = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE;
        v4l2_ioctl(e->fd, VIDIOC_STREAMOFF, &t);
    }
    for (int i = 0; i < JZ_NCAP; i++)
        if (e->cap[i]) munmap(e->cap[i], e->cap_len[i]);
    if (e->fd >= 0) v4l2_close(e->fd);
    free(e);
}

// Opens an H.264 encoder taking w x h NV12 (BT.601 limited) surfaces through `nout` DMABUF
// slots. `bitrate` in bps, `gop` in frames.
jz_enc *jz_enc_open(uint32_t w, uint32_t h, uint32_t fps, uint32_t bitrate, uint32_t gop, int nout,
                    char *err, size_t errlen) {
    jz_enc *e = calloc(1, sizeof *e);
    if (!e) return NULL;
    e->fd = -1;
    if (nout < 1 || nout > JZ_MAX_OUT) {
        errno = EINVAL;
        set_err(err, errlen, "nout");
        goto fail;
    }
    // Non-blocking: in blocking mode libnvv4l2's DQBUF waits forever when the encoder still
    // holds the only queued input, which a one-frame-per-tick caller cannot afford.
    e->fd = v4l2_open("/dev/v4l2-nvenc", O_RDWR | O_NONBLOCK);
    if (e->fd < 0) {
        set_err(err, errlen, "v4l2_open /dev/v4l2-nvenc");
        goto fail;
    }
    struct v4l2_format f;
    memset(&f, 0, sizeof f);
    f.type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE;
    f.fmt.pix_mp.pixelformat = V4L2_PIX_FMT_H264;
    f.fmt.pix_mp.width = w;
    f.fmt.pix_mp.height = h;
    f.fmt.pix_mp.num_planes = 1;
    f.fmt.pix_mp.plane_fmt[0].sizeimage = JZ_CAP_SIZE;
    if (v4l2_ioctl(e->fd, VIDIOC_S_FMT, &f) < 0) {
        set_err(err, errlen, "S_FMT capture");
        goto fail;
    }
    memset(&f, 0, sizeof f);
    f.type = V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE;
    f.fmt.pix_mp.pixelformat = V4L2_PIX_FMT_NV12M;
    f.fmt.pix_mp.width = w;
    f.fmt.pix_mp.height = h;
    f.fmt.pix_mp.num_planes = 2;
    f.fmt.pix_mp.colorspace = V4L2_COLORSPACE_SMPTE170M;
    if (v4l2_ioctl(e->fd, VIDIOC_S_FMT, &f) < 0) {
        set_err(err, errlen, "S_FMT output");
        goto fail;
    }
    // Tuning: a rejected control only costs quality, not correctness.
    ctrl(e, V4L2_CID_MPEG_VIDEO_BITRATE_MODE, V4L2_MPEG_VIDEO_BITRATE_MODE_CBR);
    ctrl(e, V4L2_CID_MPEG_VIDEO_BITRATE, bitrate);
    ctrl(e, V4L2_CID_MPEG_VIDEO_GOP_SIZE, gop);
    ctrl(e, V4L2_CID_MPEG_VIDEO_IDR_INTERVAL, gop);
    ctrl(e, V4L2_CID_MPEG_VIDEOENC_HW_PRESET_TYPE_PARAM, V4L2_ENC_HW_PRESET_ULTRAFAST);
    ctrl(e, V4L2_CID_MPEG_VIDEO_MAX_PERFORMANCE, 1);
    ctrl(e, V4L2_CID_MPEG_VIDEOENC_NUM_BFRAMES, 0);
    ctrl(e, V4L2_CID_MPEG_VIDEOENC_INSERT_SPS_PPS_AT_IDR, 1);
    ctrl(e, V4L2_CID_MPEG_VIDEOENC_INSERT_VUI, 1);
    ctrl(e, V4L2_CID_MPEG_VIDEO_H264_PROFILE, V4L2_MPEG_VIDEO_H264_PROFILE_HIGH);
    // No frame reordering: decode order == display order, so the muxer needs no ctts.
    ctrl(e, V4L2_CID_MPEG_VIDEOENC_POC_TYPE, 2);
    struct v4l2_streamparm sp;
    memset(&sp, 0, sizeof sp);
    sp.type = V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE;
    sp.parm.output.timeperframe.numerator = 1;
    sp.parm.output.timeperframe.denominator = fps;
    v4l2_ioctl(e->fd, VIDIOC_S_PARM, &sp);

    struct v4l2_requestbuffers rb;
    memset(&rb, 0, sizeof rb);
    rb.count = nout;
    rb.type = V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE;
    rb.memory = V4L2_MEMORY_DMABUF;
    if (v4l2_ioctl(e->fd, VIDIOC_REQBUFS, &rb) < 0 || (int)rb.count < nout) {
        set_err(err, errlen, "REQBUFS output");
        goto fail;
    }
    memset(&rb, 0, sizeof rb);
    rb.count = JZ_NCAP;
    rb.type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE;
    rb.memory = V4L2_MEMORY_MMAP;
    if (v4l2_ioctl(e->fd, VIDIOC_REQBUFS, &rb) < 0 || rb.count < JZ_NCAP) {
        set_err(err, errlen, "REQBUFS capture");
        goto fail;
    }
    for (int i = 0; i < JZ_NCAP; i++) {
        struct v4l2_plane pl[1];
        struct v4l2_buffer b;
        memset(&b, 0, sizeof b);
        memset(pl, 0, sizeof pl);
        b.type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE;
        b.memory = V4L2_MEMORY_MMAP;
        b.index = i;
        b.m.planes = pl;
        b.length = 1;
        if (v4l2_ioctl(e->fd, VIDIOC_QUERYBUF, &b) < 0) {
            set_err(err, errlen, "QUERYBUF");
            goto fail;
        }
        struct v4l2_exportbuffer eb;
        memset(&eb, 0, sizeof eb);
        eb.type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE;
        eb.index = i;
        if (v4l2_ioctl(e->fd, VIDIOC_EXPBUF, &eb) < 0) {
            set_err(err, errlen, "EXPBUF");
            goto fail;
        }
        // libnvv4l2 maps capture buffers through the exported fd; v4l2_mmap fails.
        void *m = mmap(NULL, pl[0].length, PROT_READ | PROT_WRITE, MAP_SHARED, eb.fd,
                       pl[0].m.mem_offset);
        close(eb.fd);
        if (m == MAP_FAILED) {
            set_err(err, errlen, "mmap capture");
            goto fail;
        }
        e->cap[i] = m;
        e->cap_len[i] = pl[0].length;
    }
    int t = V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE;
    if (v4l2_ioctl(e->fd, VIDIOC_STREAMON, &t) < 0) {
        set_err(err, errlen, "STREAMON output");
        goto fail;
    }
    t = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE;
    if (v4l2_ioctl(e->fd, VIDIOC_STREAMON, &t) < 0) {
        set_err(err, errlen, "STREAMON capture");
        goto fail;
    }
    for (int i = 0; i < JZ_NCAP; i++) {
        if (cap_buf(e, i, VIDIOC_QBUF) < 0) {
            set_err(err, errlen, "QBUF capture");
            goto fail;
        }
    }
    return e;
fail:
    jz_enc_close(e);
    return NULL;
}

// Queues `s` (an NV12 jz_alloc_nv12 surface of the encoder's size) into output slot `index`.
int jz_enc_queue(jz_enc *e, int index, NvBufSurface *s, int64_t pts_us) {
    NvBufSurfaceParams *p = &s->surfaceList[0];
    struct v4l2_plane pl[2];
    struct v4l2_buffer b;
    memset(&b, 0, sizeof b);
    memset(pl, 0, sizeof pl);
    b.type = V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE;
    b.memory = V4L2_MEMORY_DMABUF;
    b.index = index;
    b.m.planes = pl;
    b.length = 2;
    b.flags = V4L2_BUF_FLAG_TIMESTAMP_COPY;
    b.timestamp.tv_sec = pts_us / 1000000;
    b.timestamp.tv_usec = pts_us % 1000000;
    // Plane offsets are known to libnvv4l2 from the NvBufSurface behind the fd.
    for (int i = 0; i < 2; i++) {
        pl[i].m.fd = (int)p->bufferDesc;
        pl[i].bytesused = p->planeParams.psize[i];
    }
    return v4l2_ioctl(e->fd, VIDIOC_QBUF, &b);
}

// Non-blocking DQBUF answers EAGAIN until a buffer is done; poll in 0.5 ms steps.
static int dqbuf(jz_enc *e, struct v4l2_buffer *b, int timeout_ms) {
    for (int waited = 0;; waited++) {
        if (v4l2_ioctl(e->fd, VIDIOC_DQBUF, b) == 0) return 0;
        if (errno != EAGAIN) return -2;
        if (waited >= timeout_ms * 2) return -1;
        usleep(500);
    }
}

// Returns the output slot the encoder is done reading, -1 on timeout, -2 on error.
int jz_enc_reclaim(jz_enc *e, int timeout_ms) {
    struct v4l2_plane pl[2];
    struct v4l2_buffer b;
    memset(&b, 0, sizeof b);
    memset(pl, 0, sizeof pl);
    b.type = V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE;
    b.memory = V4L2_MEMORY_DMABUF;
    b.m.planes = pl;
    b.length = 2;
    int r = dqbuf(e, &b, timeout_ms);
    return r < 0 ? r : (int)b.index;
}

// Returns a capture index holding one access unit, -1 on timeout, -2 on error. Hand the index
// back with jz_enc_release once the data is copied.
int jz_enc_dequeue(jz_enc *e, int timeout_ms, const uint8_t **data, uint32_t *len, int64_t *pts_us) {
    struct v4l2_plane pl[1];
    struct v4l2_buffer b;
    memset(&b, 0, sizeof b);
    memset(pl, 0, sizeof pl);
    b.type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE;
    b.memory = V4L2_MEMORY_MMAP;
    b.m.planes = pl;
    b.length = 1;
    int r = dqbuf(e, &b, timeout_ms);
    if (r < 0) return r;
    if (b.index >= JZ_NCAP || pl[0].bytesused > e->cap_len[b.index]) return -2;
    *data = e->cap[b.index];
    *len = pl[0].bytesused;
    *pts_us = (int64_t)b.timestamp.tv_sec * 1000000 + b.timestamp.tv_usec;
    return (int)b.index;
}

int jz_enc_release(jz_enc *e, int index) { return cap_buf(e, index, VIDIOC_QBUF); }
