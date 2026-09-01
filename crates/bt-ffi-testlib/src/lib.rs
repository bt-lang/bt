//! BT FFI cross-platform test library.
//!
//! This library exports only the stable C ABI functions needed by acceptance tests;
//! it does not depend on BT production code.

use std::ffi::c_char;

/// Returns the sum of two signed 32-bit integers.
#[no_mangle]
pub extern "C" fn bt_ffi_add_i32(left: i32, right: i32) -> i32 {
    left + right
}

/// Returns the sum of two unsigned 32-bit integers.
#[no_mangle]
pub extern "C" fn bt_ffi_add_u32(left: u32, right: u32) -> u32 {
    left + right
}

/// Returns a signed 8-bit test value.
#[no_mangle]
pub extern "C" fn bt_ffi_i8() -> i8 {
    -7
}

/// Returns a signed 16-bit test value.
#[no_mangle]
pub extern "C" fn bt_ffi_i16() -> i16 {
    -300
}

/// Returns an unsigned 8-bit test value.
#[no_mangle]
pub extern "C" fn bt_ffi_u8() -> u8 {
    250
}

/// Returns an unsigned 16-bit test value.
#[no_mangle]
pub extern "C" fn bt_ffi_u16() -> u16 {
    60_000
}

/// Returns the sum of two signed 64-bit integers.
#[no_mangle]
pub extern "C" fn bt_ffi_add_i64(left: i64, right: i64) -> i64 {
    left + right
}

/// Returns the sum of two unsigned 64-bit integers.
#[no_mangle]
pub extern "C" fn bt_ffi_add_u64(left: u64, right: u64) -> u64 {
    left + right
}

/// Returns the sum of two pointer-sized signed integers.
#[no_mangle]
pub extern "C" fn bt_ffi_add_isize(left: isize, right: isize) -> isize {
    left + right
}

/// Returns the sum of two pointer-sized unsigned integers.
#[no_mangle]
pub extern "C" fn bt_ffi_add_usize(left: usize, right: usize) -> usize {
    left + right
}

/// Returns the sum of two 32-bit floating-point values.
#[no_mangle]
pub extern "C" fn bt_ffi_add_f32(left: f32, right: f32) -> f32 {
    left + right
}

/// Returns the sum of two 64-bit floating-point values.
#[no_mangle]
pub extern "C" fn bt_ffi_add_f64(left: f64, right: f64) -> f64 {
    left + right
}

/// Reports whether the supplied pointer is null.
#[no_mangle]
pub extern "C" fn bt_ffi_is_null(pointer: *const std::ffi::c_void) -> i32 {
    i32::from(pointer.is_null())
}

/// Returns a static test pointer for checking pointer returns and round trips.
#[no_mangle]
pub extern "C" fn bt_ffi_static_pointer() -> *const std::ffi::c_void {
    static VALUE: u8 = 1;
    std::ptr::addr_of!(VALUE).cast()
}

/// Returns the input pointer unchanged for checking Buffer owner inheritance.
#[no_mangle]
pub extern "C" fn bt_ffi_echo_pointer(pointer: *mut std::ffi::c_void) -> *mut std::ffi::c_void {
    pointer
}

/// Returns a stable UTF-8 C string.
#[no_mangle]
pub extern "C" fn bt_ffi_static_cstr() -> *const c_char {
    b"BT\0".as_ptr().cast()
}

/// Writes UTF-8 `BT` and a NUL byte to a writable Buffer of at least three bytes.
#[no_mangle]
pub unsafe extern "C" fn bt_ffi_write_bt(pointer: *mut u8) -> i32 {
    if pointer.is_null() {
        return 0;
    }
    // SAFETY: Acceptance tests call this with an FfiBuffer at least three bytes long;
    // the function writes exactly three bytes synchronously and does not retain the address.
    unsafe {
        *pointer = b'B';
        *pointer.add(1) = b'T';
        *pointer.add(2) = 0;
    }
    3
}

/// Checks whether the input equals the UTF-8 C string `BT`.
#[no_mangle]
pub unsafe extern "C" fn bt_ffi_is_bt_text(text: *const c_char) -> i32 {
    if text.is_null() {
        return 0;
    }
    // SAFETY: The caller guarantees that `text` points to a readable, NUL-terminated C string;
    // this reads only three bytes, checks the terminator at the third byte, and retains no address.
    unsafe {
        i32::from(*text == b'B' as c_char && *text.add(1) == b'T' as c_char && *text.add(2) == 0)
    }
}

/// Accepts a no-return-value call for testing `void()` return semantics.
#[no_mangle]
pub extern "C" fn bt_ffi_noop() {}
