// Slint compile step: ui/app.slint -> $OUT_DIR/app.rs (23 MB / ~230 k lines),
// pulled into the crate by `slint::include_modules!()` in src/main.rs.
//
// Rebuild trigger: this script does NOT re-run when a .rs file changes.
// `slint_build::compile` already emits the precise `cargo:rerun-if-changed`
// lines for everything that can change the generated code -- ui/app.slint, the
// four imported fonts (assets/fonts/*.otf, ui/fonts/*.otf), the tray icon
// (assets/icons/o11a1.png) and the SLINT_* environment variables -- so a
// `touch src/main.rs` costs 0 s here and only the rustc invocation runs.
// Verified by reading $OUT_DIR/../output and by timing a touch-build: the
// build script process does not even start. Do not add a bare
// `rerun-if-changed=.` or similar: it would make every .rs edit pay the ~7 s
// Slint codegen (and, worse, make cargo rebuild the crate as if app.rs changed).
//
// The 23 MB of generated code is also why the crate is codegen-bound; see the
// measured numbers in Cargo.toml ([profile.release.package.o11a1]) and
// tools/build.ps1.
fn main() {
    if let Err(err) = slint_build::compile("ui/app.slint") {
        eprintln!("slint build failed: {err}");
        std::process::exit(1);
    }
}
