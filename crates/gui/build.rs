fn main() {
    #[cfg(target_os = "windows")]
    {
        println!("cargo:rerun-if-changed=app.manifest");
        println!("cargo:rerun-if-changed=assets/app.ico");

        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/app.ico");
        res.set_manifest_file("app.manifest");
        res.set("ProductName", "GameAccel");
        res.set("FileDescription", "GameAccel QUIC/UDP Accelerator");
        res.set("LegalCopyright", "Copyright (c) GameAccel");
        res.compile().expect("failed to compile Windows resources");
    }
}
