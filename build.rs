fn main() {
    // Locate vcpkg installation (same directory used by libheif-sys).
    let vcpkg_root = std::path::Path::new("target/vcpkg/installed/x64-windows-static-md");

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
}
