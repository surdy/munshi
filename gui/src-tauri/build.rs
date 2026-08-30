use std::fs;
use std::path::Path;

fn main() {
    stage_cli_placeholder();
    tauri_build::build();
}

/// Ensures `resources/bin/munshi` exists so Tauri's resource collection can run.
///
/// Packaging stages the real CLI here first (`contrib/build-gui.sh`), and that copy is what ships.
/// A plain `cargo build` or `cargo test` in this crate has no reason to build the whole CLI first,
/// so an empty placeholder stands in. It is deliberately left non-executable: `resolve.rs` only
/// accepts a bundled binary carrying an execute bit, so a placeholder is correctly treated as
/// "this build ships no CLI" and the app falls back to an installed one.
fn stage_cli_placeholder() {
    let resource = Path::new("resources/bin/munshi");
    println!("cargo:rerun-if-changed=resources/bin/munshi");
    if resource.exists() {
        return;
    }
    if let Some(directory) = resource.parent() {
        let _ = fs::create_dir_all(directory);
    }
    if fs::write(resource, b"").is_ok() {
        println!(
            "cargo:warning=no munshi CLI staged at resources/bin/munshi; \
             using an empty placeholder. Run contrib/build-gui.sh to package a real bundle."
        );
    }
}
