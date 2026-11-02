fn main() {
    let root = std::path::Path::new("../../vendor/rnnoise-native");
    let mut build = cc::Build::new();
    build
        .include(root.join("include"))
        .include(root.join("src"))
        .define("RNNOISE_BUILD", None)
        .warnings(false);
    for file in [
        "denoise.c",
        "rnn.c",
        "pitch.c",
        "kiss_fft.c",
        "celt_lpc.c",
        "nnet.c",
        "nnet_default.c",
        "parse_lpcnet_weights.c",
        "rnnoise_data.c",
        "rnnoise_tables.c",
    ] {
        build.file(root.join("src").join(file));
    }
    println!("cargo:rerun-if-changed={}", root.display());
    build.compile("relay_rnnoise_native");
}
