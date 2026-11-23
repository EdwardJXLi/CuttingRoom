use std::{env, path::PathBuf};

use libloading::Library;

pub(super) fn open(names: &[&str]) -> Option<Library> {
    let directory = match env::var_os("CUTTINGROOM_CODEC_DIR") {
        Some(path) => PathBuf::from(path),
        None => env::current_exe().ok()?.parent()?.join("codecs"),
    };
    if !directory.is_absolute() {
        return None;
    }
    names.iter().find_map(|name| {
        let path = directory.join(name);
        // SAFETY: Load only named codec libraries from the configured native library
        // directory. Their initialization runs on the thumbnail worker.
        unsafe {
            #[cfg(windows)]
            {
                use libloading::os::windows::{
                    LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
                };

                libloading::os::windows::Library::load_with_flags(
                    path,
                    LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
                )
                .ok()
                .map(Library::from)
            }
            #[cfg(not(windows))]
            {
                Library::new(path).ok()
            }
        }
    })
}

/// Load a function pointer while the decoder owns the library.
///
/// # Safety
/// The caller must match the codec's C signature and keep the library loaded
/// for all uses of the returned pointer.
pub(super) unsafe fn symbol<T: Copy>(library: &Library, name: &[u8]) -> Option<T> {
    // SAFETY: The caller supplies the matching signature and owns the library.
    unsafe { library.get::<T>(name) }.ok().map(|symbol| *symbol)
}
