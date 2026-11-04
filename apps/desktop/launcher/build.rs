fn main() {
    // The portable single-file exe is the launcher, so its embedded icon is
    // what Explorer shows. Without this, Windows falls back to a generic icon.
    #[cfg(windows)]
    {
        let mut res = tauri_winres::WindowsResource::new();
        res.set_icon("../src-tauri/icons/icon.ico");
        res.set("ProductName", "TUIC Proxy");
        res.set("FileDescription", "TUIC 代理客户端");
        if let Ok(version) = std::env::var("CARGO_PKG_VERSION") {
            res.set("FileVersion", &version);
            res.set("ProductVersion", &version);
        }
        if let Err(err) = res.compile() {
            panic!("failed to embed Windows resources: {err}");
        }
    }
}
