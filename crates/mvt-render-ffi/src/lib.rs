//! C ABI surface for the vector tile renderer, used by the Swift binding.
//!
//! Mirrors the wasm and JNI bindings: the caller passes every fetched tile
//! concatenated into one buffer plus a length table, and gets PNG bytes back.
//! Nothing calls back out of Rust.
//!
//! # Ownership
//!
//! Anything this module returns by pointer is Rust-allocated and must be
//! handed back to the matching free function — [`mvt_string_free`] for
//! `char*`, [`mvt_buffer_free`] for byte buffers, [`mvt_renderer_free`] for the
//! renderer. Freeing them with `free()` is undefined behaviour.
//!
//! Every entry point catches unwinds: this ships inside someone else's app, so
//! a panic must become an error return rather than an abort.

use std::ffi::{c_char, c_int, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;

use mvt_render::Renderer;

/// Opaque handle to a renderer.
pub struct MvtRenderer {
    inner: Renderer,
}

pub const MVT_OK: c_int = 0;
pub const MVT_ERR_NULL_HANDLE: c_int = -1;
pub const MVT_ERR_BAD_ARGUMENT: c_int = -2;
pub const MVT_ERR_RENDER_FAILED: c_int = -3;
pub const MVT_ERR_PANIC: c_int = -4;

/// Copies a Rust string out as a C string the caller owns.
fn into_c_string(value: String) -> *mut c_char {
    match CString::new(value) {
        Ok(owned) => owned.into_raw(),
        // A NUL byte can only come from style content; report it as absence
        // rather than crashing.
        Err(_) => ptr::null_mut(),
    }
}

/// # Safety
/// `text` must be a valid NUL-terminated C string.
unsafe fn borrow_str<'a>(text: *const c_char) -> Option<&'a str> {
    if text.is_null() {
        return None;
    }
    CStr::from_ptr(text).to_str().ok()
}

/// Parses a style and returns a renderer handle, or null on failure.
///
/// On failure, and if `error_out` is non-null, it receives an owned message
/// that must be released with [`mvt_string_free`].
///
/// # Safety
/// `style_json` must be a valid NUL-terminated UTF-8 C string. `error_out` must
/// be null or point to writable storage for one pointer.
#[no_mangle]
pub unsafe extern "C" fn mvt_renderer_new(
    style_json: *const c_char,
    error_out: *mut *mut c_char,
) -> *mut MvtRenderer {
    if !error_out.is_null() {
        *error_out = ptr::null_mut();
    }
    let result = catch_unwind(AssertUnwindSafe(|| {
        let Some(style) = borrow_str(style_json) else {
            return Err("style_json is null or not valid UTF-8".to_string());
        };
        Renderer::new(style).map_err(|error| error.to_string())
    }));

    match result {
        Ok(Ok(inner)) => Box::into_raw(Box::new(MvtRenderer { inner })),
        Ok(Err(message)) => {
            if !error_out.is_null() {
                *error_out = into_c_string(message);
            }
            ptr::null_mut()
        }
        Err(_) => {
            if !error_out.is_null() {
                *error_out = into_c_string("vector tile renderer panicked".to_string());
            }
            ptr::null_mut()
        }
    }
}

/// Releases a renderer. Passing null is a no-op.
///
/// # Safety
/// `handle` must come from [`mvt_renderer_new`] and not already be freed.
#[no_mangle]
pub unsafe extern "C" fn mvt_renderer_free(handle: *mut MvtRenderer) {
    if handle.is_null() {
        return;
    }
    drop(Box::from_raw(handle));
}

/// Replaces the style. Fetched vector tiles stay valid — only the paint changes.
///
/// # Safety
/// `handle` must be live; `style_json` a valid NUL-terminated UTF-8 C string.
#[no_mangle]
pub unsafe extern "C" fn mvt_renderer_set_style(
    handle: *mut MvtRenderer,
    style_json: *const c_char,
    error_out: *mut *mut c_char,
) -> c_int {
    if !error_out.is_null() {
        *error_out = ptr::null_mut();
    }
    if handle.is_null() {
        return MVT_ERR_NULL_HANDLE;
    }
    let result = catch_unwind(AssertUnwindSafe(|| {
        let Some(style) = borrow_str(style_json) else {
            return Err((MVT_ERR_BAD_ARGUMENT, "style_json is null or not valid UTF-8".to_string()));
        };
        (*handle)
            .inner
            .set_style(style)
            .map_err(|error| (MVT_ERR_BAD_ARGUMENT, error.to_string()))
    }));

    match result {
        Ok(Ok(())) => MVT_OK,
        Ok(Err((code, message))) => {
            if !error_out.is_null() {
                *error_out = into_c_string(message);
            }
            code
        }
        Err(_) => MVT_ERR_PANIC,
    }
}

/// Returns the fetch plan as a JSON array, or null on failure. Release with
/// [`mvt_string_free`].
///
/// # Safety
/// `handle` must be live.
#[no_mangle]
pub unsafe extern "C" fn mvt_renderer_plan(
    handle: *mut MvtRenderer,
    z: u8,
    x: u32,
    y: u32,
) -> *mut c_char {
    if handle.is_null() {
        return ptr::null_mut();
    }
    let result = catch_unwind(AssertUnwindSafe(|| {
        let plan = (*handle).inner.plan(z, x, y);
        let entries: Vec<String> = plan
            .requests
            .iter()
            .map(|request| {
                format!(
                    r#"{{"sourceId":{},"url":{},"z":{},"x":{},"y":{},"scale":{},"offsetX":{},"offsetY":{}}}"#,
                    json_string(&request.source_id),
                    json_string(&request.url),
                    request.z,
                    request.x,
                    request.y,
                    request.scale,
                    request.offset_x,
                    request.offset_y,
                )
            })
            .collect();
        format!("[{}]", entries.join(","))
    }));

    match result {
        Ok(json) => into_c_string(json),
        Err(_) => ptr::null_mut(),
    }
}

/// Reasons the current style may not render as intended, as a JSON array of
/// strings. Release with [`mvt_string_free`].
///
/// # Safety
/// `handle` must be live.
#[no_mangle]
pub unsafe extern "C" fn mvt_renderer_diagnostics(handle: *mut MvtRenderer) -> *mut c_char {
    if handle.is_null() {
        return ptr::null_mut();
    }
    let result = catch_unwind(AssertUnwindSafe(|| {
        let messages: Vec<String> = (*handle)
            .inner
            .diagnostics()
            .iter()
            .map(|message| json_string(message))
            .collect();
        format!("[{}]", messages.join(","))
    }));
    match result {
        Ok(json) => into_c_string(json),
        Err(_) => ptr::null_mut(),
    }
}

/// Layer `type` values in the current style that will not be drawn, as a JSON
/// array of strings. Release with [`mvt_string_free`].
///
/// # Safety
/// `handle` must be live.
#[no_mangle]
pub unsafe extern "C" fn mvt_renderer_unsupported_layer_types(
    handle: *mut MvtRenderer,
) -> *mut c_char {
    if handle.is_null() {
        return ptr::null_mut();
    }
    let result = catch_unwind(AssertUnwindSafe(|| {
        let kinds: Vec<String> = (*handle)
            .inner
            .unsupported_layer_types()
            .iter()
            .map(|kind| json_string(kind))
            .collect();
        format!("[{}]", kinds.join(","))
    }));
    match result {
        Ok(json) => into_c_string(json),
        Err(_) => ptr::null_mut(),
    }
}

/// Rasterises one tile into a newly allocated PNG buffer.
///
/// `data` is every fetched source tile concatenated in plan order; `lengths`
/// gives each tile's byte length, `0` marking one that failed to fetch. On
/// success `out_ptr`/`out_len` receive a buffer to release with
/// [`mvt_buffer_free`].
///
/// # Safety
/// `handle` must be live. `data` must be readable for `data_len` bytes and
/// `lengths` for `lengths_len` elements. `out_ptr` and `out_len` must be
/// writable.
#[no_mangle]
pub unsafe extern "C" fn mvt_renderer_render(
    handle: *mut MvtRenderer,
    z: u8,
    x: u32,
    y: u32,
    tile_size: u32,
    data: *const u8,
    data_len: usize,
    lengths: *const u32,
    lengths_len: usize,
    out_ptr: *mut *mut u8,
    out_len: *mut usize,
) -> c_int {
    if handle.is_null() {
        return MVT_ERR_NULL_HANDLE;
    }
    if out_ptr.is_null() || out_len.is_null() {
        return MVT_ERR_BAD_ARGUMENT;
    }
    *out_ptr = ptr::null_mut();
    *out_len = 0;

    let result = catch_unwind(AssertUnwindSafe(|| {
        let data_bytes: &[u8] = if data.is_null() || data_len == 0 {
            &[]
        } else {
            std::slice::from_raw_parts(data, data_len)
        };
        let length_table: &[u32] = if lengths.is_null() || lengths_len == 0 {
            &[]
        } else {
            std::slice::from_raw_parts(lengths, lengths_len)
        };

        let mut slices: Vec<Option<&[u8]>> = Vec::with_capacity(length_table.len());
        let mut offset = 0usize;
        for length in length_table {
            let length = *length as usize;
            if length == 0 {
                slices.push(None);
                continue;
            }
            let end = match offset.checked_add(length) {
                Some(end) if end <= data_bytes.len() => end,
                _ => return Err(MVT_ERR_BAD_ARGUMENT),
            };
            slices.push(Some(&data_bytes[offset..end]));
            offset = end;
        }

        (*handle)
            .inner
            .render(z, x, y, tile_size, &slices)
            .map_err(|_| MVT_ERR_RENDER_FAILED)
    }));

    match result {
        Ok(Ok(png)) => {
            let mut boxed = png.into_boxed_slice();
            *out_len = boxed.len();
            *out_ptr = boxed.as_mut_ptr();
            // Ownership moves to the caller until mvt_buffer_free.
            std::mem::forget(boxed);
            MVT_OK
        }
        Ok(Err(code)) => code,
        Err(_) => MVT_ERR_PANIC,
    }
}

/// Tessellates `z/x/y` into the packed float buffer described by
/// [`mvt_render::Renderer::tessellate_packed`].
///
/// On success `*out_ptr` owns `*out_len` **floats** (not bytes) and must be
/// released with [`mvt_floats_free`].
///
/// # Safety
/// Same contract as [`mvt_renderer_render`]: `data`/`lengths` must be valid for
/// their lengths, and the out params must be writable.
#[no_mangle]
#[cfg(feature = "tessellate")]
pub unsafe extern "C" fn mvt_renderer_tessellate(
    handle: *mut MvtRenderer,
    z: u8,
    x: u32,
    y: u32,
    tile_size: u32,
    data: *const u8,
    data_len: usize,
    lengths: *const u32,
    lengths_len: usize,
    out_ptr: *mut *mut f32,
    out_len: *mut usize,
) -> c_int {
    if handle.is_null() {
        return MVT_ERR_NULL_HANDLE;
    }
    if out_ptr.is_null() || out_len.is_null() {
        return MVT_ERR_BAD_ARGUMENT;
    }
    *out_ptr = ptr::null_mut();
    *out_len = 0;

    let result = catch_unwind(AssertUnwindSafe(|| {
        let data_bytes: &[u8] = if data.is_null() || data_len == 0 {
            &[]
        } else {
            std::slice::from_raw_parts(data, data_len)
        };
        let length_table: &[u32] = if lengths.is_null() || lengths_len == 0 {
            &[]
        } else {
            std::slice::from_raw_parts(lengths, lengths_len)
        };

        let mut slices: Vec<Option<&[u8]>> = Vec::with_capacity(length_table.len());
        let mut offset = 0usize;
        for length in length_table {
            let length = *length as usize;
            if length == 0 {
                slices.push(None);
                continue;
            }
            let end = match offset.checked_add(length) {
                Some(end) if end <= data_bytes.len() => end,
                _ => return Err(MVT_ERR_BAD_ARGUMENT),
            };
            slices.push(Some(&data_bytes[offset..end]));
            offset = end;
        }

        (*handle)
            .inner
            .tessellate_packed(z, x, y, tile_size, &slices)
            .map_err(|_| MVT_ERR_RENDER_FAILED)
    }));

    match result {
        Ok(Ok(packed)) => {
            let mut boxed = packed.into_boxed_slice();
            *out_len = boxed.len();
            *out_ptr = boxed.as_mut_ptr();
            std::mem::forget(boxed);
            MVT_OK
        }
        Ok(Err(code)) => code,
        Err(_) => MVT_ERR_PANIC,
    }
}

/// Encodes straight-alpha RGBA pixels as PNG.
///
/// Exists because the platform encoders are slow enough to dominate a GPU tile:
/// on device this is ~7 ms where `UIImage`/`Bitmap.compress` take ~48 ms.
///
/// # Safety
/// `rgba` must be valid for `width * height * 4` bytes.
#[no_mangle]
pub unsafe extern "C" fn mvt_encode_png(
    rgba: *const u8,
    width: u32,
    height: u32,
    out_ptr: *mut *mut u8,
    out_len: *mut usize,
) -> c_int {
    if out_ptr.is_null() || out_len.is_null() {
        return MVT_ERR_BAD_ARGUMENT;
    }
    *out_ptr = ptr::null_mut();
    *out_len = 0;

    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4));
    let Some(expected) = expected else {
        return MVT_ERR_BAD_ARGUMENT;
    };
    if rgba.is_null() || expected == 0 {
        return MVT_ERR_BAD_ARGUMENT;
    }

    let result = catch_unwind(AssertUnwindSafe(|| {
        let pixels = std::slice::from_raw_parts(rgba, expected);
        mvt_render::encode::encode_png_rgba(width, height, pixels)
            .map_err(|_| MVT_ERR_RENDER_FAILED)
    }));

    match result {
        Ok(Ok(png)) => {
            let mut boxed = png.into_boxed_slice();
            *out_len = boxed.len();
            *out_ptr = boxed.as_mut_ptr();
            std::mem::forget(boxed);
            MVT_OK
        }
        Ok(Err(code)) => code,
        Err(_) => MVT_ERR_PANIC,
    }
}

/// Releases a float buffer returned by [`mvt_renderer_tessellate`].
///
/// # Safety
/// `ptr`/`len` must be exactly what a tessellate call produced, freed once.
#[no_mangle]
#[cfg(feature = "tessellate")]
pub unsafe extern "C" fn mvt_floats_free(ptr: *mut f32, len: usize) {
    if ptr.is_null() || len == 0 {
        return;
    }
    drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len)));
}

/// Releases a buffer returned by [`mvt_renderer_render`].
///
/// # Safety
/// `ptr`/`len` must be exactly what a render call produced, freed once.
#[no_mangle]
pub unsafe extern "C" fn mvt_buffer_free(ptr: *mut u8, len: usize) {
    if ptr.is_null() || len == 0 {
        return;
    }
    drop(Box::from_raw(std::slice::from_raw_parts_mut(ptr, len)));
}

/// Releases a string returned by this module.
///
/// # Safety
/// `text` must come from this module and be freed once.
#[no_mangle]
pub unsafe extern "C" fn mvt_string_free(text: *mut c_char) {
    if text.is_null() {
        return;
    }
    drop(CString::from_raw(text));
}

fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
