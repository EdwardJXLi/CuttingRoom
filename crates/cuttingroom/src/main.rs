#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use slint::ComponentHandle;

#[cfg(target_os = "macos")]
mod macos {
    #[used]
    // SAFETY: This read-only Mach-O section contains the standalone app's property list bytes.
    #[unsafe(link_section = "__TEXT,__info_plist")]
    static INFO_PLIST: [u8; include_bytes!("../Info.plist").len()] =
        *include_bytes!("../Info.plist");
}

slint::include_modules!();

fn main() -> Result<(), slint::PlatformError> {
    let window = MainWindow::new()?;
    window.set_app_version(env!("CARGO_PKG_VERSION").into());
    window.run()
}
