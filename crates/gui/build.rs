fn main() {
    #[cfg(target_os = "windows")]
    {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
        let manifest_path = std::path::Path::new(&manifest_dir).join("app.manifest");
        println!("cargo:rerun-if-changed=app.manifest");
        println!("cargo:rustc-link-arg-bin=game-accel=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg-bin=game-accel=/MANIFESTUAC:NO");
        println!(
            "cargo:rustc-link-arg-bin=game-accel=/MANIFESTINPUT:{}",
            manifest_path.display()
        );
    }
}
