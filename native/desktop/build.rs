use std::path::PathBuf;

fn main() {
    let icon = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../assets/app-icon.ico");
    println!("cargo:rerun-if-changed={}", icon.display());

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon(
            icon.to_str()
                .expect("SanctuaryPlayer icon path must be valid UTF-8"),
        );
        resource
            .compile()
            .expect("failed to embed SanctuaryPlayer Windows resources");
    }
}
