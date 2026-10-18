//! The WebAssembly interface: the page writes a request into memory it
//! allocated with `pg_alloc`, calls `pg_call` and reads the response at the
//! returned pointer (`pg_result_len` bytes). After a trap, `pg_panic`
//! points at the panic message (`pg_result_len` bytes).

#![allow(unsafe_code)]

use std::cell::RefCell;

thread_local! {
    static RESULT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    static PANIC: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

#[unsafe(no_mangle)]
pub extern "C" fn pg_alloc(len: usize) -> *mut u8 {
    let mut v = Vec::<u8>::with_capacity(len.max(1));
    let p = v.as_mut_ptr();
    std::mem::forget(v);
    p
}

/// # Safety
/// `ptr` and `len` must come from one `pg_alloc` call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pg_free(ptr: *mut u8, len: usize) {
    // SAFETY: the caller passes back an allocation of `pg_alloc`.
    drop(unsafe { Vec::from_raw_parts(ptr, 0, len.max(1)) });
}

/// # Safety
/// The pointers must address `pg_alloc`ated memory holding UTF-8 text of the
/// given lengths.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pg_call(
    m: *const u8,
    m_len: usize,
    b: *const u8,
    b_len: usize,
) -> *const u8 {
    static HOOK: std::sync::Once = std::sync::Once::new();
    HOOK.call_once(|| {
        std::panic::set_hook(Box::new(|info| {
            PANIC.with(|p| *p.borrow_mut() = info.to_string().into_bytes());
        }))
    });
    // SAFETY: the caller guarantees the buffers.
    let (method, body) =
        unsafe { (std::slice::from_raw_parts(m, m_len), std::slice::from_raw_parts(b, b_len)) };
    let out = crate::handle(&String::from_utf8_lossy(method), &String::from_utf8_lossy(body));
    RESULT.with(|r| {
        let mut r = r.borrow_mut();
        *r = out.into_bytes();
        r.as_ptr()
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn pg_result_len() -> usize {
    RESULT.with(|r| r.borrow().len())
}

#[unsafe(no_mangle)]
pub extern "C" fn pg_panic() -> *const u8 {
    PANIC.with(|p| {
        let p = p.borrow();
        RESULT.with(|r| *r.borrow_mut() = p.clone());
        RESULT.with(|r| r.borrow().as_ptr())
    })
}
