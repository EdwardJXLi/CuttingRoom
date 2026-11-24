#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use std::{env, error::Error, path::PathBuf, process::ExitCode};

mod app;
mod grid;
mod image_decode;
mod metadata;
mod preview;
mod source_file;

#[cfg(target_os = "macos")]
mod macos {
    #[used]
    // SAFETY: This read-only Mach-O section contains the standalone app's property list bytes.
    #[unsafe(link_section = "__TEXT,__info_plist")]
    static INFO_PLIST: [u8; include_bytes!("../Info.plist").len()] =
        *include_bytes!("../Info.plist");
}

slint::include_modules!();

fn main() -> Result<ExitCode, Box<dyn Error>> {
    let mut args = env::args_os().skip(1);
    let folder = args.next();
    if args.next().is_some() {
        eprintln!("Usage: cuttingroom [FOLDER]");
        return Ok(ExitCode::FAILURE);
    }
    if folder
        .as_deref()
        .is_some_and(|arg| arg == "--help" || arg == "-h")
    {
        println!("Usage: cuttingroom [FOLDER]");
        return Ok(ExitCode::SUCCESS);
    }

    app::run(folder.map(PathBuf::from))
}
