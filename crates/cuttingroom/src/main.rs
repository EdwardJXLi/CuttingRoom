#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use slint::ComponentHandle;

slint::include_modules!();

fn main() -> Result<(), slint::PlatformError> {
    MainWindow::new()?.run()
}
