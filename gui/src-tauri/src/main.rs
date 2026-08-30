// Keep the console window off on Windows release builds. Munshi targets macOS and Linux, but the
// attribute is harmless and stops a stray console if anyone ever builds it there.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    munshi_gui_lib::run();
}
