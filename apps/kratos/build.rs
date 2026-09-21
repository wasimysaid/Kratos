fn main() {
    println!("cargo:rerun-if-changed=../../dist/windows/kratos.rc");
    println!("cargo:rerun-if-changed=../../dist/windows/kratos.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile_for(
            "../../dist/windows/kratos.rc",
            &["kratos"],
            embed_resource::NONE,
        )
        .manifest_required()
        .expect("Windows app icon resource compilation failed");
    }
}
