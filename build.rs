//! Embeds the web UI (`web/index.html`, one self-contained file).

fn main() {
    let ui = std::path::PathBuf::from("web/index.html");
    println!("cargo:rerun-if-changed={}", ui.display());
    let path = std::fs::canonicalize(&ui).expect("web/index.html");
    println!("cargo:rustc-env=OMT_UI_HTML={}", path.display());
}
