// When built with `--features indexstore` on macOS, add a runtime rpath to the
// Swift toolchain's lib dir so the dynamic loader can find libIndexStore.dylib
// (its install name is @rpath/libIndexStore.dylib). Link search/lib come from the
// codegraph-indexstore build script; the rpath must be set on the BINARY here.
fn main() {
    if std::env::var("CARGO_FEATURE_INDEXSTORE").is_ok()
        && std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos")
    {
        if let Ok(out) = std::process::Command::new("xcrun").args(["--find", "swift"]).output() {
            if out.status.success() {
                let swift = String::from_utf8_lossy(&out.stdout);
                if let Some(usr) = std::path::Path::new(swift.trim()).ancestors().nth(2) {
                    println!("cargo:rustc-link-arg=-Wl,-rpath,{}", usr.join("lib").display());
                }
            }
        }
    }
}
