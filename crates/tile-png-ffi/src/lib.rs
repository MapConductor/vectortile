//! PNG encoding for `ios-sdk-core`.
//!
//! The C ABI twin of `tile-png-jni`, and deliberately as small: the core links
//! into every app that uses the SDK, so nothing beyond the `png` crate ships
//! here.
//!
//! Panics never cross the boundary — unwinding into Swift is undefined
//! behaviour — so failures come back as a null pointer and the caller falls
//! back to `UIImage.pngData()`.

use std::os::raw::c_int;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;

pub const TILE_PNG_OK: c_int = 0;
pub const TILE_PNG_ERR_BAD_ARGUMENT: c_int = -1;
pub const TILE_PNG_ERR_ENCODE_FAILED: c_int = -2;
pub const TILE_PNG_ERR_PANIC: c_int = -3;

/// Encodes an RGBA8 buffer as PNG.
///
/// When `premultiplied` is non-zero the buffer is un-premultiplied in place
/// first, which mutates the caller's memory; the caller must not reuse it.
/// On success `*out_ptr` owns `*out_len` bytes, released with
/// [`tile_png_buffer_free`].
///
/// # Safety
/// `rgba` must be valid for `width * height * 4` bytes, and the out params
/// must be writable.
#[no_mangle]
pub unsafe extern "C" fn tile_png_encode(
    rgba: *mut u8,
    width: u32,
    height: u32,
    premultiplied: c_int,
    out_ptr: *mut *mut u8,
    out_len: *mut usize,
) -> c_int {
    if out_ptr.is_null() || out_len.is_null() {
        return TILE_PNG_ERR_BAD_ARGUMENT;
    }
    *out_ptr = ptr::null_mut();
    *out_len = 0;

    let Some(expected) = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
    else {
        return TILE_PNG_ERR_BAD_ARGUMENT;
    };
    if rgba.is_null() || expected == 0 {
        return TILE_PNG_ERR_BAD_ARGUMENT;
    }

    let result = catch_unwind(AssertUnwindSafe(|| {
        let pixels = std::slice::from_raw_parts_mut(rgba, expected);
        if premultiplied != 0 {
            tile_png::encode_png_premultiplied(width, height, pixels)
        } else {
            tile_png::encode_png_rgba(width, height, pixels)
        }
    }));

    match result {
        Ok(Ok(png)) => {
            let mut boxed = png.into_boxed_slice();
            *out_len = boxed.len();
            *out_ptr = boxed.as_mut_ptr();
            // Ownership moves to the caller until tile_png_buffer_free.
            std::mem::forget(boxed);
            TILE_PNG_OK
        }
        Ok(Err(_)) => TILE_PNG_ERR_ENCODE_FAILED,
        Err(_) => TILE_PNG_ERR_PANIC,
    }
}

/// Releases a buffer returned by [`tile_png_encode`].
///
/// # Safety
/// `ptr`/`len` must be exactly what an encode call produced, freed once.
#[no_mangle]
pub unsafe extern "C" fn tile_png_buffer_free(ptr: *mut u8, len: usize) {
    if ptr.is_null() || len == 0 {
        return;
    }
    drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len)));
}
