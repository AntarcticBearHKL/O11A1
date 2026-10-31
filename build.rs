// Slint compile step: ui/app.slint -> $OUT_DIR/app.rs (23 MB / ~230 k lines),
// pulled into the crate by `slint::include_modules!()` in src/main.rs.
//
// Rebuild trigger: this script does NOT re-run when a .rs file changes.
// `slint_build::compile` already emits the precise `cargo:rerun-if-changed`
// lines for everything that can change the generated code -- ui/app.slint, the
// two imported fonts (assets/fonts/*.ttf, the UI face's Regular + Bold), the
// tray icon (assets/icons/o11a1.png) and the SLINT_* environment variables -- so
// a `touch src/main.rs` costs 0 s here and only the rustc invocation runs.
// Verified by reading $OUT_DIR/../output and by timing a touch-build: the
// build script process does not even start. Do not add a bare
// `rerun-if-changed=.` or similar: it would make every .rs edit pay the ~7 s
// Slint codegen (and, worse, make cargo rebuild the crate as if app.rs changed).
//
// The 23 MB of generated code is also why the crate is codegen-bound; see the
// measured numbers in Cargo.toml ([profile.release.package.o11a1]) and
// tools/build.ps1.
fn main() {
    // f178: the executable's own icon. `winres` writes the ICON resource into
    // the .exe, so Explorer, the taskbar, Alt-Tab and the tray (which loads the
    // same resource by id) all show the same artwork. Rerun only when the .ico
    // changes; a missing icon is a hard error on purpose - a release without
    // its icon is worse than a failed build.
    println!("cargo:rerun-if-changed=assets/icons/o11a1.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winres::WindowsResource::new();
        res.set_icon("assets/icons/o11a1.ico");
        if let Err(err) = res.compile() {
            eprintln!("icon resource failed: {err}");
            std::process::exit(1);
        }
    }
    if let Err(err) = slint_build::compile("ui/app.slint") {
        eprintln!("slint build failed: {err}");
        std::process::exit(1);
    }
}
