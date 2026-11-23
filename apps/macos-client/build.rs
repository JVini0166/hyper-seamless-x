fn main() {
    println!("cargo:rerun-if-changed=src/video_toolbox.c");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    cc::Build::new()
        .file("src/video_toolbox.c")
        .compile("hsx_video_toolbox");
    for framework in ["CoreFoundation", "CoreMedia", "CoreVideo", "VideoToolbox"] {
        println!("cargo:rustc-link-lib=framework={framework}");
    }
}
