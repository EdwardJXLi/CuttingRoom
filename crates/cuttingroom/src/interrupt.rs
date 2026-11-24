use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub(crate) fn install() -> Result<Arc<AtomicBool>, ctrlc::Error> {
    let interrupted = Arc::new(AtomicBool::new(false));
    #[cfg(windows)]
    if !attach_console() {
        return Ok(interrupted);
    }

    let signal = Arc::clone(&interrupted);
    ctrlc::set_handler(move || {
        if !signal.swap(true, Ordering::Relaxed) {
            // Winit tags quit requests with the generation of the running event loop.
            let _ = slint::invoke_from_event_loop(|| {
                let _ = slint::quit_event_loop();
            });
        }
    })?;
    Ok(interrupted)
}

#[cfg(windows)]
fn attach_console() -> bool {
    use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole, GetConsoleCP};

    // SAFETY: These calls take no pointers. Attach only to an existing parent console.
    unsafe { AttachConsole(ATTACH_PARENT_PROCESS) != 0 || GetConsoleCP() != 0 }
}
