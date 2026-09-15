fn main() {
    // Provision the vendored OBS runtime + extprocess_recorder.exe.
    //  1) next to the built exe (target/<profile>/libobs) so `cargo run`/dev works,
    //  2) into src-tauri/libobs, a stable path the installer bundles as a resource.
    if let Err(e) = build_helper::Builder::new().build() {
        println!("cargo:warning=build-helper (profile dir) failed: {e}");
    }
    if let Err(e) = build_helper::Builder::new().with_path(".").build() {
        println!("cargo:warning=build-helper (resource dir) failed: {e}");
    }
    tauri_build::build();
}
