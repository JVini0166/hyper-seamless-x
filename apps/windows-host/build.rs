fn main() {
    println!("cargo:rerun-if-changed=src/media_foundation.cpp");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    cc::Build::new()
        .cpp(true)
        .std("c++17")
        .flag_if_supported("/EHsc")
        .file("src/media_foundation.cpp")
        .compile("hsx_media_foundation");
    for library in ["mfplat", "mfuuid", "ole32", "wmcodecdspuuid"] {
        println!("cargo:rustc-link-lib={library}");
    }
}
