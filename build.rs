fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=cpp/vhid_shim.cpp");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    let vendor = std::path::Path::new("vendor/karabiner-vhid");
    if !vendor.join("include").exists() {
        panic!("vendor/karabiner-vhid is missing: run `git submodule update --init`");
    }
    cc::Build::new()
        .cpp(true)
        .file("cpp/vhid_shim.cpp")
        .include(vendor.join("include"))
        .include(vendor.join("vendor/vendor/include"))
        .flag("-std=c++23")
        .flag("-Wno-deprecated-declarations")
        .opt_level(2)
        .compile("ferry_vhid");
    println!("cargo:rustc-link-lib=c++");
    println!("cargo:rustc-link-lib=framework=CoreFoundation");
    println!("cargo:rustc-link-lib=framework=IOKit");
}
