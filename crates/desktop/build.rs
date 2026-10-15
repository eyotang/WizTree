fn main() {
    println!("cargo:rerun-if-changed=assets/WizTree.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set_icon("assets/WizTree.ico")
            .set("ProductName", "WizTree")
            .set("FileDescription", "WizTree")
            .set("OriginalFilename", "WizTree.exe")
            .set("CompanyName", "")
            .set("LegalCopyright", "WizTree contributors")
            .compile()
            .expect("Unable to embed WizTree application icon");
    }
}
