//! Reading data files, also on WebAssembly.
//!
//! On `wasm32-unknown-unknown` there is no file system: [`read`] asks the
//! page through two imports. `env.synth_fs_read(path, len)` returns the
//! length of the file at `path` (UTF-8), or a negative number if there is
//! none; `env.synth_fs_take(dst)` then copies it to `dst`. The playground's
//! worker answers them with synchronous fetches from the web server.

use std::io;
use std::path::Path;

/// The contents of a file.
pub fn read(path: &Path) -> io::Result<Vec<u8>> {
    imp::read(path)
}

/// The contents of a UTF-8 text file.
pub fn read_to_string(path: &Path) -> io::Result<String> {
    String::from_utf8(read(path)?).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(not(target_arch = "wasm32"))]
mod imp {
    pub fn read(path: &std::path::Path) -> std::io::Result<Vec<u8>> {
        std::fs::read(path)
    }
}

#[cfg(target_arch = "wasm32")]
#[allow(unsafe_code)]
mod imp {
    use std::io;

    #[link(wasm_import_module = "env")]
    unsafe extern "C" {
        fn synth_fs_read(path: *const u8, len: usize) -> f64;
        fn synth_fs_take(dst: *mut u8);
    }

    pub fn read(path: &std::path::Path) -> io::Result<Vec<u8>> {
        let p = path.to_string_lossy();
        // SAFETY: the import reads `len` bytes of `p`.
        let n = unsafe { synth_fs_read(p.as_ptr(), p.len()) };
        if n < 0.0 {
            return Err(io::Error::new(io::ErrorKind::NotFound, format!("{p}: not found")));
        }
        let mut buf = vec![0u8; n as usize];
        // SAFETY: the import writes the `n` bytes it announced to `buf`.
        unsafe { synth_fs_take(buf.as_mut_ptr()) };
        Ok(buf)
    }
}
