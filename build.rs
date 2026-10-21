fn main() {
    if let Err(err) = slint_build::compile("ui/app.slint") {
        eprintln!("slint build failed: {err}");
        std::process::exit(1);
    }
}
