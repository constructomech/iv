fn main() {
    println!("cargo:rerun-if-changed=src/libraw_wrapper.c");
    println!("cargo:rerun-if-changed=src/ffmpeg_wrapper.c");
    println!("cargo:rerun-if-changed=src/heif_wrapper.c");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        build_unix();
        return;
    }

    // Locate vcpkg installations.
    let vcpkg_root = std::path::Path::new("target/vcpkg/installed/x64-windows-static-md");
    let ffmpeg_root = std::path::Path::new("target/vcpkg/installed/x64-windows");

    // Link LibRaw and its transitive dependencies as static libraries.
    println!(
        "cargo:rustc-link-search=native={}",
        vcpkg_root.join("lib").display()
    );
    println!("cargo:rustc-link-lib=static=raw_r");
    println!("cargo:rustc-link-lib=static=lcms2");
    println!("cargo:rustc-link-lib=static=zlib");
    println!("cargo:rustc-link-lib=static=jasper");
    println!("cargo:rustc-link-lib=static=jpeg");

    // Compile our thin C wrapper that calls libraw with correct params.
    let mut cc = cc::Build::new();
    cc.file("src/libraw_wrapper.c");
    cc.include(vcpkg_root.join("include"));
    cc.compile("iv_libraw");

    // Compile the FFmpeg wrapper against dynamic vcpkg headers. The wrapper
    // loads FFmpeg DLLs with LoadLibrary, so we intentionally do not link the
    // main executable against FFmpeg import libraries.
    let mut ffmpeg_cc = cc::Build::new();
    ffmpeg_cc.file("src/ffmpeg_wrapper.c");
    ffmpeg_cc.include(ffmpeg_root.join("include"));
    ffmpeg_cc.compile("iv_ffmpeg");

    // Compile the HEIF wrapper against dynamic vcpkg headers. Like FFmpeg,
    // libheif is runtime-loaded to keep LGPL components replaceable.
    let mut heif_cc = cc::Build::new();
    heif_cc.file("src/heif_wrapper.c");
    heif_cc.include(ffmpeg_root.join("include"));
    heif_cc.compile("iv_heif");
}

fn build_unix() {
    let is_macos = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos");
    let wrappers: &[(&str, &str, &[&str])] = &[
        ("libraw", "iv_libraw", &["libraw_r"]),
        (
            "ffmpeg",
            "iv_ffmpeg",
            &["libavcodec", "libavformat", "libavutil", "libswscale"],
        ),
        ("heif", "iv_heif", &["libheif"]),
    ];
    for &(source, archive, libraries) in wrappers {
        let mut build = cc::Build::new();
        build.file(format!("src/{source}_wrapper.c"));
        for library in libraries {
            // Some LibRaw .pc files name GNU's C++ runtime even on macOS.
            let macos_libraw = is_macos && *library == "libraw_r";
            let config = pkg_config::Config::new()
                .statik(false)
                .cargo_metadata(!macos_libraw)
                .probe(library)
                .unwrap_or_else(|err| {
                    panic!("Could not find {library}; install the native dependencies in README.md: {err}")
                });
            if macos_libraw {
                for path in &config.link_paths {
                    println!("cargo:rustc-link-search=native={}", path.display());
                }
                for lib in &config.libs {
                    let lib = if lib == "stdc++" { "c++" } else { lib };
                    println!("cargo:rustc-link-lib={lib}");
                }
            }
            build.includes(config.include_paths);
        }
        build.compile(archive);
    }
}
