//! Stages the self-contained Windows App SDK runtime, embeds the
//! deployment manifest and gives the executable its icon.

use std::path::{Path, PathBuf};

/// File name of the icon, in `assets` and next to the executable.
const ICON: &str = "syspilot.ico";

/// The directory the executable is built into. `OUT_DIR` is somewhere
/// under `<profile>/build/`; how deep depends on the cargo version.
fn profile_directory(out_dir: &Path) -> Option<&Path> {
    out_dir.ancestors().find(|directory| directory.ends_with("build"))?.parent()
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets");
    windows_reactor_setup::as_self_contained();

    // The icon as a resource: what Explorer, the taskbar and shortcuts show.
    embed_resource::compile("assets/app.rc", embed_resource::NONE)
        .manifest_required()
        .expect("(icon resource): expected assets/app.rc to compile with the Windows SDK's rc.exe");

    // And as a file beside the executable: WinUI takes a window icon only
    // by path.
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    let beside_executable = profile_directory(&out_dir)
        .expect("(OUT_DIR): expected <profile>/build/<package>/out")
        .join(ICON);
    std::fs::copy(Path::new("assets").join(ICON), &beside_executable)
        .expect("(icon): expected to copy assets/syspilot.ico next to the executable");
}
