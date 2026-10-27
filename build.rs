fn main() {
    // The frontend is embedded into the binary, so a change under `web/` has
    // to rebuild it. Without this, `tauri_build` only watches the config and
    // the capabilities, and the shipped UI silently keeps the old version.
    println!("cargo:rerun-if-changed=web");
    tauri_build::build()
}
