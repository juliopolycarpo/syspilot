//! Embeds the manifest that selects Common Controls 6, without which the
//! task dialog does not exist, and gives the executable the app's icon.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets");
    println!("cargo:rerun-if-changed=../ui/assets/syspilot.ico");

    let manifest = std::path::absolute("assets/companion.manifest")
        .expect("(manifest): expected assets/companion.manifest to have an absolute path");
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}", manifest.display());

    embed_resource::compile("assets/companion.rc", embed_resource::NONE)
        .manifest_required()
        .expect("(icon resource): expected assets/companion.rc to compile with the Windows SDK's rc.exe");
}
