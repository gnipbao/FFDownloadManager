fn main() {
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(
        tauri_build::AppManifest::new().commands(&[
            "desktop_snapshot",
            "desktop_create",
            "desktop_filename",
            "desktop_resolve_media",
            "desktop_create_media",
            "desktop_pause",
            "desktop_resume",
            "desktop_remove",
            "desktop_reveal",
            "desktop_pause_all",
            "desktop_open_folder",
            "desktop_copy_link",
            "desktop_info",
        ]),
    ))
    .expect("build desktop assets and permissions");
}
