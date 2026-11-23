//! Stages the self-contained Windows App SDK runtime and embeds the
//! deployment manifest.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    windows_reactor_setup::as_self_contained();
}
