fn main() {
    // Copy the vendored OBS runtime + extprocess_recorder.exe next to the built
    // executable (target/<profile>/libobs). Runs before tauri_build so the
    // resources exist when bundling.
    if let Err(e) = build_helper::Builder::new().build() {
        // Don't hard-fail dev builds without the OBS toolchain wired; the app
        // falls back to the noop recorder at runtime.
        println!("cargo:warning=build-helper (OBS provisioning) failed: {e}");
    }
    tauri_build::build();
}
