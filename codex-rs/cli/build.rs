fn main() {
    println!("cargo:rustc-check-cfg=cfg(codex_local)");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg=-ObjC");
    }
}
