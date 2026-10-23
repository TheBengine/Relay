fn main() {
    println!("cargo:rerun-if-changed=probe.cpp");
    println!("cargo:rerun-if-env-changed=RELAY_PROBE_VARIANT");
    println!("cargo:rerun-if-env-changed=RELAY_TEST_CRYPTO_LIBRARY");
    println!("cargo:rerun-if-env-changed=RELAY_TEST_CRYPTO_SOURCE");
    println!("cargo:rerun-if-env-changed=LK_CUSTOM_WEBRTC");
    let sdk = std::path::PathBuf::from(std::env::var("LK_CUSTOM_WEBRTC").unwrap());
    let mut build = cc::Build::new();
    if let Ok(source) = std::env::var("RELAY_TEST_CRYPTO_SOURCE") {
        println!("cargo:rerun-if-changed={source}");
        build.file(source);
    }
    if let Ok(variant) = std::env::var("RELAY_PROBE_VARIANT") {
        build.define(
            if variant == "rng" {
                "RELAY_PROBE_RNG_FAILURE"
            } else {
                "RELAY_PROBE_QUOTA"
            },
            None,
        );
    }
    build
        .cpp(true)
        .file("probe.cpp")
        .include(sdk.join("include"))
        .include(sdk.join("include/third_party/abseil-cpp"))
        .include(sdk.join("include/third_party/boringssl/src/include"))
        .define("RTC_ENABLE_H265", None)
        .define("WEBRTC_USE_H264", None)
        .define("WEBRTC_WIN", None)
        .define("NOMINMAX", None)
        .define("WIN32_LEAN_AND_MEAN", None)
        .flag("/std:c++20")
        .flag("/EHsc")
        .compile("relay_owned_probe");
    if let Ok(library) = std::env::var("RELAY_TEST_CRYPTO_LIBRARY") {
        println!("cargo:rustc-link-arg={library}");
    } else {
        println!(
            "cargo:rustc-link-search=native={}",
            sdk.join("lib").display()
        );
        println!("cargo:rustc-link-lib=static=webrtc");
    }
    for lib in [
        "msdmo",
        "wmcodecdspuuid",
        "dmoguids",
        "crypt32",
        "iphlpapi",
        "ole32",
        "secur32",
        "winmm",
        "ws2_32",
        "strmiids",
        "d3d11",
        "gdi32",
        "dxgi",
        "dwmapi",
        "shcore",
    ] {
        println!("cargo:rustc-link-lib={lib}");
    }
}
