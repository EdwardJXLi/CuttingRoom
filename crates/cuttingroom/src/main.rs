#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use slint::ComponentHandle;

slint::include_modules!();

fn main() -> Result<(), slint::PlatformError> {
    let window = MainWindow::new()?;
    window.set_app_version(env!("CARGO_PKG_VERSION").into());
    window.run()
}
