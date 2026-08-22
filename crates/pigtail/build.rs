fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set_icon("packaging/windows/pigtail.ico")
            .compile()
            .expect("embedding Windows icon resource");
    }
}
