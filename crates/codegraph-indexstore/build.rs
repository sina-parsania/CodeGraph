// Link libIndexStore (ships in the active Xcode/Swift toolchain) only on macOS.
// The crate is an opt-in dependency (CLI `indexstore` feature), so this runs only
// when someone actually builds that feature — i.e. on a Mac with the toolchain.
fn main() {
    let link = std::env::var("CARGO_FEATURE_LINK").is_ok();
    if link && std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        if let Ok(out) = std::process::Command::new("xcrun")
            .args(["--find", "swift"])
            .output()
        {
            if out.status.success() {
                let swift = String::from_utf8_lossy(&out.stdout);
                // .../usr/bin/swift -> .../usr/lib (where libIndexStore.dylib lives)
                if let Some(usr) = std::path::Path::new(swift.trim()).ancestors().nth(2) {
                    println!(
                        "cargo:rustc-link-search=native={}",
                        usr.join("lib").display()
                    );
                }
            }
        }
        println!("cargo:rustc-link-lib=dylib=IndexStore");
    }
}
