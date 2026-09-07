fn main() {
    // 產生最小合法 ICO 到 OUT_DIR，避免把品牌/外部 binary 資產混入儲存庫。
    // Tauri Windows resource compiler 要求 icon，即使 bundle.active=false。
    let out_dir = std::env::var_os("OUT_DIR").expect("OUT_DIR");
    let icon_path = std::path::PathBuf::from(out_dir).join("generated-icon.ico");
    std::fs::write(&icon_path, minimal_icon()).expect("write generated icon");
    let windows = tauri_build::WindowsAttributes::new().window_icon_path(icon_path);
    tauri_build::try_build(tauri_build::Attributes::new().windows_attributes(windows))
        .expect("failed to run tauri build");
}

fn minimal_icon() -> [u8; 70] {
    // 1x1 BGRA DIB + 1-row AND mask。
    [
        0, 0, 1, 0, 1, 0, // ICONDIR
        1, 1, 0, 0, 1, 0, 32, 0, 48, 0, 0, 0, 22, 0, 0, 0, // entry
        40, 0, 0, 0, // BITMAPINFOHEADER size
        1, 0, 0, 0, 2, 0, 0, 0, // width, height (DIB doubles icon height)
        1, 0, 32, 0, // planes, bpp
        0, 0, 0, 0, // compression
        4, 0, 0, 0, // image size
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // resolution and palette
        0xC0, 0xA0, 0x60, 0xFF, // one BGRA pixel
        0, 0, 0, 0, // AND mask row, DWORD aligned
    ]
}
