//! Windows: embed the app icon in `goro.exe`.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let icon =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/icons/goro.ico");
        println!("cargo:rerun-if-changed={}", icon.display());
        let rc = std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("goro.rc");
        std::fs::write(
            &rc,
            format!(
                "1 ICON \"{}\"\n",
                icon.display().to_string().replace('\\', "\\\\")
            ),
        )
        .unwrap();
        embed_resource::compile(&rc, embed_resource::NONE)
            .manifest_optional()
            .unwrap();
    }
}
