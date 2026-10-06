fn main() {
    // Listing the commands generates `allow-*` permissions, so only capabilities grant them.
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(
        tauri_build::AppManifest::new().commands(&["runtime_status", "restart_runtime", "open_log"]),
    ))
    .expect("tauri build configuration");
}
