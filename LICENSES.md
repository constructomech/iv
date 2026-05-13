# Third-party components

`iv` is MIT-licensed (see `LICENSE`) and ships under that license. It
relies on a number of third-party native libraries that are dynamically
loaded at runtime — `iv.exe` itself does not statically link any LGPL or
GPL code. Distributed Windows builds must ship the runtime DLLs listed
below alongside `iv.exe`. Each of those libraries retains its own license,
summarized here for convenience and to support LGPL § 6 source-availability
obligations when iv is redistributed.

The vcpkg port at `vcpkg-overlay/libheif/` and the dependency pins in
`Cargo.toml` and `target/vcpkg` are the authoritative source-of-truth for
component versions.

## Runtime DLLs (Windows, x64-windows triplet)

| File | License | Source | Notes |
|---|---|---|---|
| `heif.dll` | LGPL-3.0-only | https://github.com/strukturag/libheif | HEIC/HEIF container + plugin host. Built from `vcpkg-overlay/libheif/` (adds `ffmpeg-decoder` feature). |
| `libde265.dll` | LGPL-3.0-only | https://github.com/strukturag/libde265 | Default HEVC decoder used by `heif.dll`. Loaded automatically as an OS DLL import of `heif.dll`. |
| `aom.dll` | BSD-2-Clause | https://aomedia.googlesource.com/aom | AV1 decoder/encoder. Used by `heif.dll` for AV1-coded HEIFs and by `iv-bench` for synthetic AV1 HEIC fixtures. |
| `avcodec-62.dll` | LGPL-2.1-or-later | https://github.com/FFmpeg/FFmpeg | FFmpeg codec library. Used directly by iv for video thumbnails, and by `heif.dll`'s FFmpeg HEVC decoder plugin. |
| `avformat-62.dll` | LGPL-2.1-or-later | https://github.com/FFmpeg/FFmpeg | FFmpeg muxer/demuxer library. Used for video container parsing. |
| `avutil-60.dll` | LGPL-2.1-or-later | https://github.com/FFmpeg/FFmpeg | FFmpeg shared utilities. |
| `swresample-6.dll` | LGPL-2.1-or-later | https://github.com/FFmpeg/FFmpeg | FFmpeg audio resampling library. iv doesn't use audio, but `avcodec-62.dll` has it as a load-time DLL import, so it must be present alongside it. |
| `swscale-9.dll` | LGPL-2.1-or-later | https://github.com/FFmpeg/FFmpeg | FFmpeg image scaling/colorspace library. |

`avdevice-62.dll`, `avfilter-11.dll`, and `pkgconf-7.dll` appear in the
vcpkg `installed/x64-windows/bin/` directory but are not required by
`iv.exe` or `heif.dll` at runtime.

## Statically linked into `iv.exe`

| Library | License | Source | Notes |
|---|---|---|---|
| LibRaw (`raw_r`) | LGPL-2.1-only OR CDDL-1.0 | https://github.com/LibRaw/LibRaw | RAW image decoder. Statically linked under the CDDL-1.0 option (CDDL is permissive enough to allow our static-md vcpkg triplet). |
| Little CMS (`lcms2`) | MIT | https://github.com/mm2/Little-CMS | Color-management library, transitively required by LibRaw. |
| zlib | zlib | https://github.com/madler/zlib | Compression library, transitively required by LibRaw. |
| JasPer | JasPer (MIT-style) | https://github.com/jasper-software/jasper | JPEG-2000 decoder, transitively required by LibRaw. |
| libjpeg-turbo (`jpeg`) | IJG / BSD-style | https://github.com/libjpeg-turbo/libjpeg-turbo | JPEG codec, transitively required by LibRaw. |

## Components excluded by design

| Component | License | Why excluded |
|---|---|---|
| `x265` (HEVC encoder) | GPL-2.0-or-later | We never encode HEVC. The bench's HEIC fixture path uses AV1 (aom). The libheif vcpkg port's default `hevc` feature pulls x265 in; our overlay disables it by default and the README install command explicitly opts out via `[core,...]`. |
| `x264` (AVC encoder) | GPL-2.0-or-later | Not used. Not in the libheif install set. |
| FFmpeg `--enable-libx265` / `--enable-libx264` / `--enable-libfdk-aac` | GPL or non-free | Not enabled in the vcpkg ffmpeg port we install. The vcpkg port's default features include only LGPL-compatible components. |

If you modify `vcpkg-overlay/libheif/vcpkg.json` or the `vcpkg install`
command in `README.md` to add the `hevc` or `x264` features, you will
introduce GPL components into the runtime DLL set and `iv.exe`'s
distribution becomes subject to GPL terms. Don't do that without
intentionally accepting that license obligation.

## `iv-thumb` (separate workspace)

`iv-thumb` is a developer tool that lives outside the main workspace and
is built with its own vcpkg tree. It deliberately links the GPL `x265`
HEVC encoder via libheif's `hevc` feature so it can write HEVC thumbnails
into existing image files. This means `iv-thumb.exe` is GPL-2.0-or-later.
It is never linked into `iv.exe`. See `iv-thumb/LICENSE` for details.

## Source obligations

`iv` itself is distributed under the MIT license. For the LGPL-licensed
DLLs in the runtime set, LGPL § 6 requires that source be available. The
relevant sources are pinned by:

- `Cargo.toml`'s `[package.metadata.vcpkg]` section (vcpkg revision)
- `vcpkg-overlay/libheif/portfile.cmake` (libheif version + patches)
- The vcpkg ports tree under `target/vcpkg/ports/` (libde265, ffmpeg, aom,
  and their transitive dependencies)

Together these uniquely identify the source revision of every library
shipped with a given iv build. When redistributing iv binaries, you should
either bundle the source archives or provide a written offer per LGPL § 6.
