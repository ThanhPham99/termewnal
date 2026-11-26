fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "windows" {
        let icon_path = "../../assets/termewnal-icon.ico";
        println!("cargo:rerun-if-changed={icon_path}");
        let mut res = winresource::WindowsResource::new();
        res.set_icon(icon_path);
        res.set("ProductName", "Termewnal");
        res.set(
            "FileDescription",
            "Termewnal Terminal & Multi-Protocol Workspace",
        );
        if let Err(err) = res.compile() {
            println!("cargo:warning=failed to embed Windows icon resources: {err}");
        }
    }
}
