//! Embed the application manifest (themed common controls, per-monitor DPI awareness)
//! using the MSVC linker directly, so no resource compiler is needed.

fn main() {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("assets")
        .join("fz-maximize.manifest");
    println!("cargo:rerun-if-changed={}", manifest.display());
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_env == "msvc" {
        // Only the binary gets the manifest; test harnesses are unaffected.
        println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}", manifest.display());
    }
}
