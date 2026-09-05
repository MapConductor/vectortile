//! PNG encoding for `android-sdk-core`.
//!
//! Deliberately the smallest library that can carry an encoder across JNI. The
//! core is linked into every app that uses the SDK, so what ships here is what
//! every consumer pays for: nothing beyond the `png` crate and the JNI glue.
//!
//! Panics never cross the boundary. A panic unwinding into the JVM is undefined
//! behaviour, so failures come back as a null array and the caller falls back
//! to `Bitmap.compress`.

use jni::objects::{JByteBuffer, JClass};
use jni::sys::{jbyteArray, jint};
use jni::JNIEnv;
use std::panic::{catch_unwind, AssertUnwindSafe};

/// Encodes a **direct** RGBA8 `ByteBuffer` as PNG, returning null on failure.
///
/// The buffer must be direct: the pointer crosses JNI as-is, with no copy. When
/// `premultiplied` is true the buffer is un-premultiplied in place first, which
/// mutates the caller's memory — the caller must not reuse it afterwards.
///
/// # Safety
/// Called only by the JVM through the `TilePngEncoder` declaration.
#[no_mangle]
pub extern "system" fn Java_com_mapconductor_core_tileserver_TilePngEncoder_nativeEncode(
    mut env: JNIEnv,
    _class: JClass,
    buffer: JByteBuffer,
    width: jint,
    height: jint,
    premultiplied: bool,
) -> jbyteArray {
    let result = catch_unwind(AssertUnwindSafe(|| {
        if width <= 0 || height <= 0 {
            return None;
        }
        let expected = (width as usize).checked_mul(height as usize)?.checked_mul(4)?;

        let address = env.get_direct_buffer_address(&buffer).ok()?;
        let capacity = env.get_direct_buffer_capacity(&buffer).ok()?;
        if address.is_null() || capacity < expected {
            return None;
        }
        let pixels = unsafe { std::slice::from_raw_parts_mut(address, expected) };

        let png = if premultiplied {
            tile_png::encode_png_premultiplied(width as u32, height as u32, pixels)
        } else {
            tile_png::encode_png_rgba(width as u32, height as u32, pixels)
        }
        .ok()?;

        let array = env.byte_array_from_slice(&png).ok()?;
        Some(array.into_raw())
    }));

    match result {
        Ok(Some(array)) => array,
        // Null rather than an exception: the caller has a working fallback, and
        // a failed optimisation should not become an app crash.
        _ => std::ptr::null_mut(),
    }
}
