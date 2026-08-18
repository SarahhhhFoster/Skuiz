fn main() {
    // The shim is Objective-C against AudioToolbox, so it only builds for
    // Apple targets. Everything else gets the Rust C ABI alone.
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "macos" && target_os != "ios" {
        return;
    }

    println!("cargo:rerun-if-changed=shim/SkuizAudioUnit.m");
    println!("cargo:rerun-if-changed=shim/SkuizAudioUnit.h");

    cc::Build::new()
        .file("shim/SkuizAudioUnit.m")
        .flag("-fobjc-arc")
        // Block signatures are fixed by the AUv3 API, so unused parameters
        // are unavoidable rather than a smell.
        .flag("-Wno-unused-parameter")
        .compile("skuizauv3shim");

    println!("cargo:rustc-link-lib=framework=Foundation");
    println!("cargo:rustc-link-lib=framework=AudioToolbox");
    println!("cargo:rustc-link-lib=framework=AVFoundation");
}
