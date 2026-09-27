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

/// Rebuilds the plan's per-source slices from the flat buffer the caller packs.
///
/// Every entry point takes tiles the same way -- one buffer, one length table,
/// `0` for a tile that failed to fetch -- so the unpacking lives here rather
/// than once per function.
///
/// # Safety
/// `data` must be readable for `data_len` bytes and `lengths` for `lengths_len`
/// elements, and both must stay alive for as long as the returned slices.
unsafe fn unpack_tiles<'a>(
    data: *const u8,
    data_len: usize,
    lengths: *const u32,
    lengths_len: usize,
) -> Result<Vec<Option<&'a [u8]>>, c_int> {
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
    Ok(slices)
}

/// Hands a Rust byte buffer to the caller, to be released with
/// [`mvt_buffer_free`].
///
/// # Safety
/// `out_ptr` and `out_len` must be writable.
unsafe fn hand_over(bytes: Vec<u8>, out_ptr: *mut *mut u8, out_len: *mut usize) {
    let mut boxed = bytes.into_boxed_slice();
    *out_len = boxed.len();
    *out_ptr = boxed.as_mut_ptr();
    // Ownership moves to the caller until mvt_buffer_free.
    std::mem::forget(boxed);
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
    // dp one tile covers where the host shows it. Not the pixel count a render
    // is asked for: a label pass draws at twice the pixels, and a 256 dp tile
    // is still 256 dp however many pixels it carries. Sets the size the style
    // draws at and the zoom its expressions are read at. 512 is the usual
    // answer; 0 means "use it".
    display_tile_size: u32,
    error_out: *mut *mut c_char,
) -> *mut MvtRenderer {
    if !error_out.is_null() {
        *error_out = ptr::null_mut();
    }
    let result = catch_unwind(AssertUnwindSafe(|| {
        let Some(style) = borrow_str(style_json) else {
            return Err("style_json is null or not valid UTF-8".to_string());
        };
        Renderer::new(style)
            .map(|renderer| {
                if display_tile_size == 0 {
                    renderer
                } else {
                    renderer.with_display_tile_size(display_tile_size)
                }
            })
            .map_err(|error| error.to_string())
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
            return Err((
                MVT_ERR_BAD_ARGUMENT,
                "style_json is null or not valid UTF-8".to_string(),
            ));
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
                    r#"{{"sourceId":{},"url":{},"z":{},"x":{},"y":{},"scale":{},"offsetX":{},"offsetY":{},"labelsOnly":{}}}"#,
                    json_string(&request.source_id),
                    json_string(&request.url),
                    request.z,
                    request.x,
                    request.y,
                    request.scale,
                    request.offset_x,
                    request.offset_y,
                    request.labels_only,
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
        let slices = unpack_tiles(data, data_len, lengths, lengths_len)?;

        (*handle)
            .inner
            .render(z, x, y, tile_size, &slices)
            .map_err(|_| MVT_ERR_RENDER_FAILED)
    }));

    match result {
        Ok(Ok(png)) => {
            hand_over(png, out_ptr, out_len);
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
        let slices = unpack_tiles(data, data_len, lengths, lengths_len)?;

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

/// Credits the style's sources ask to be shown, as a JSON array of strings.
/// Release with [`mvt_string_free`].
///
/// The host must display these: a style is data under someone's licence. The
/// text may contain HTML, because a credit is normally a link to the licence.
///
/// # Safety
/// `handle` must be live.
#[no_mangle]
pub unsafe extern "C" fn mvt_renderer_attributions(handle: *mut MvtRenderer) -> *mut c_char {
    json_array_from(handle, |renderer| renderer.attributions())
}

/// The style's `glyphs` URL template, or null when it names none.
/// Release with [`mvt_string_free`].
///
/// # Safety
/// `handle` must be live.
#[no_mangle]
pub unsafe extern "C" fn mvt_renderer_glyphs_url_template(handle: *mut MvtRenderer) -> *mut c_char {
    if handle.is_null() {
        return ptr::null_mut();
    }
    let result = catch_unwind(AssertUnwindSafe(|| {
        (*handle).inner.glyphs_url_template().map(str::to_owned)
    }));
    match result {
        Ok(Some(template)) => into_c_string(template),
        _ => ptr::null_mut(),
    }
}

/// URLs of the glyph ranges this tile's labels need and the store has not got,
/// as a JSON array of strings. Release with [`mvt_string_free`].
///
/// Empty when the style names no `glyphs` template, or when everything needed
/// is already loaded. Tiles are passed exactly as for [`mvt_renderer_render`].
///
/// # Safety
/// `handle` must be live; `data`/`lengths` valid for their lengths.
#[no_mangle]
pub unsafe extern "C" fn mvt_renderer_needed_glyphs(
    handle: *mut MvtRenderer,
    z: u8,
    x: u32,
    y: u32,
    data: *const u8,
    data_len: usize,
    lengths: *const u32,
    lengths_len: usize,
) -> *mut c_char {
    if handle.is_null() {
        return ptr::null_mut();
    }
    let result = catch_unwind(AssertUnwindSafe(|| {
        let Some(template) = (*handle).inner.glyphs_url_template().map(str::to_owned) else {
            return String::from("[]");
        };
        let Ok(slices) = unpack_tiles(data, data_len, lengths, lengths_len) else {
            return String::from("[]");
        };
        let urls: Vec<String> = (*handle)
            .inner
            .needed_glyphs(z, x, y, &slices)
            .iter()
            .map(|request| json_string(&request.url(&template)))
            .collect();
        format!("[{}]", urls.join(","))
    }));
    match result {
        Ok(json) => into_c_string(json),
        Err(_) => ptr::null_mut(),
    }
}

/// Adds one glyph range PBF. Returns how many glyphs it carried, or a negative
/// error code.
///
/// # Safety
/// `handle` must be live and `pbf` readable for `pbf_len` bytes.
#[no_mangle]
pub unsafe extern "C" fn mvt_renderer_add_glyphs(
    handle: *mut MvtRenderer,
    pbf: *const u8,
    pbf_len: usize,
) -> c_int {
    if handle.is_null() {
        return MVT_ERR_NULL_HANDLE;
    }
    if pbf.is_null() || pbf_len == 0 {
        return MVT_ERR_BAD_ARGUMENT;
    }
    let result = catch_unwind(AssertUnwindSafe(|| {
        let bytes = std::slice::from_raw_parts(pbf, pbf_len);
        (*handle)
            .inner
            .add_glyphs(bytes)
            .map(|count| count as c_int)
            .unwrap_or(MVT_ERR_RENDER_FAILED)
    }));
    result.unwrap_or(MVT_ERR_PANIC)
}

/// `1` when any glyph has been loaded, `0` when none, negative on failure.
///
/// # Safety
/// `handle` must be live.
#[no_mangle]
pub unsafe extern "C" fn mvt_renderer_has_glyphs(handle: *mut MvtRenderer) -> c_int {
    bool_call(handle, |renderer| renderer.has_glyphs())
}

/// The sprite sheet's `.json` and `.png` URLs at `pixel_ratio`, as a JSON array
/// of two strings, or `[]` when the style names no sprite. Release with
/// [`mvt_string_free`].
///
/// # Safety
/// `handle` must be live.
#[no_mangle]
pub unsafe extern "C" fn mvt_renderer_sprite_urls(
    handle: *mut MvtRenderer,
    pixel_ratio: u32,
) -> *mut c_char {
    if handle.is_null() {
        return ptr::null_mut();
    }
    let result = catch_unwind(AssertUnwindSafe(|| {
        let urls: Vec<String> = match (*handle).inner.sprite_urls(pixel_ratio.max(1)) {
            Some(request) => vec![
                json_string(&request.json_url),
                json_string(&request.png_url),
            ],
            None => Vec::new(),
        };
        format!("[{}]", urls.join(","))
    }));
    match result {
        Ok(json) => into_c_string(json),
        Err(_) => ptr::null_mut(),
    }
}

/// Adds the fetched sprite sheet. Returns how many icons it carried, or a
/// negative error code.
///
/// # Safety
/// `handle` must be live, `json` a valid NUL-terminated UTF-8 C string, and
/// `png` readable for `png_len` bytes.
#[no_mangle]
pub unsafe extern "C" fn mvt_renderer_add_sprite(
    handle: *mut MvtRenderer,
    json: *const c_char,
    png: *const u8,
    png_len: usize,
) -> c_int {
    if handle.is_null() {
        return MVT_ERR_NULL_HANDLE;
    }
    let Some(index) = borrow_str(json) else {
        return MVT_ERR_BAD_ARGUMENT;
    };
    if png.is_null() || png_len == 0 {
        return MVT_ERR_BAD_ARGUMENT;
    }
    let result = catch_unwind(AssertUnwindSafe(|| {
        let sheet = std::slice::from_raw_parts(png, png_len);
        (*handle)
            .inner
            .add_sprite(index, sheet)
            .map(|count| count as c_int)
            .unwrap_or(MVT_ERR_RENDER_FAILED)
    }));
    result.unwrap_or(MVT_ERR_PANIC)
}

/// `1` when the style names a sprite the renderer has not been given yet.
///
/// # Safety
/// `handle` must be live.
#[no_mangle]
pub unsafe extern "C" fn mvt_renderer_needs_sprite(handle: *mut MvtRenderer) -> c_int {
    bool_call(handle, |renderer| renderer.needs_sprite())
}

/// `1` when this tile has to be drawn on the CPU because the style paints
/// something the GPU path cannot -- today, a patterned fill.
///
/// # Safety
/// `handle` must be live; `data`/`lengths` valid for their lengths.
#[no_mangle]
pub unsafe extern "C" fn mvt_renderer_needs_cpu(
    handle: *mut MvtRenderer,
    z: u8,
    data: *const u8,
    data_len: usize,
    lengths: *const u32,
    lengths_len: usize,
) -> c_int {
    if handle.is_null() {
        return MVT_ERR_NULL_HANDLE;
    }
    let result = catch_unwind(AssertUnwindSafe(|| {
        let Ok(slices) = unpack_tiles(data, data_len, lengths, lengths_len) else {
            return MVT_ERR_BAD_ARGUMENT;
        };
        c_int::from((*handle).inner.needs_cpu(z, &slices))
    }));
    result.unwrap_or(MVT_ERR_PANIC)
}

/// Rasterises the ground alone -- fills, lines and circles, no labels or icons.
///
/// The half of a split layer that a GPU draws and glyphs never invalidate.
/// Output and ownership match [`mvt_renderer_render`].
///
/// # Safety
/// Same contract as [`mvt_renderer_render`].
#[no_mangle]
pub unsafe extern "C" fn mvt_renderer_render_geometry(
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
        let slices = unpack_tiles(data, data_len, lengths, lengths_len)?;
        (*handle)
            .inner
            .render_geometry(z, x, y, tile_size, &slices)
            .map_err(|_| MVT_ERR_RENDER_FAILED)
    }));

    match result {
        Ok(Ok(png)) => {
            hand_over(png, out_ptr, out_len);
            MVT_OK
        }
        Ok(Err(code)) => code,
        Err(_) => MVT_ERR_PANIC,
    }
}

/// Draws the labels and icons alone, on a transparent ground, into a newly
/// allocated **premultiplied** RGBA buffer of `tile_size * tile_size * 4`
/// bytes.
///
/// The other half of a split layer: a CPU draws this while a GPU draws the
/// ground, and a font arriving refreshes only this one. `*out_placed` receives
/// how many labels were placed; when that is zero nothing is allocated,
/// `*out_ptr` stays null and the caller should serve a transparent tile.
///
/// Premultiplied, not straight: encoding it as straight alpha turns every
/// halo grey.
///
/// # Safety
/// `handle` must be live, `data`/`lengths` valid for their lengths, and
/// `out_ptr`, `out_len`, `out_placed` writable.
#[no_mangle]
pub unsafe extern "C" fn mvt_renderer_render_labels(
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
    out_placed: *mut usize,
) -> c_int {
    if handle.is_null() {
        return MVT_ERR_NULL_HANDLE;
    }
    if out_ptr.is_null() || out_len.is_null() || out_placed.is_null() {
        return MVT_ERR_BAD_ARGUMENT;
    }
    *out_ptr = ptr::null_mut();
    *out_len = 0;
    *out_placed = 0;

    let result = catch_unwind(AssertUnwindSafe(|| {
        let slices = unpack_tiles(data, data_len, lengths, lengths_len)?;
        (*handle)
            .inner
            .render_labels_rgba(z, x, y, tile_size, &slices)
            .map_err(|_| MVT_ERR_RENDER_FAILED)
    }));

    match result {
        Ok(Ok((pixels, placed))) => {
            *out_placed = placed;
            if !pixels.is_empty() {
                hand_over(pixels, out_ptr, out_len);
            }
            MVT_OK
        }
        Ok(Err(code)) => code,
        Err(_) => MVT_ERR_PANIC,
    }
}

/// Draws labels and icons over pixels the caller already has -- a GPU readback,
/// typically -- in place. Returns how many labels were placed, or a negative
/// error code.
///
/// `rgba` must hold exactly `tile_size * tile_size * 4` bytes and is read as
/// **straight** alpha, which is what a `glReadPixels` result is.
///
/// # Safety
/// `handle` must be live, `rgba` readable *and writable* for `rgba_len` bytes,
/// and `data`/`lengths` valid for their lengths.
#[no_mangle]
pub unsafe extern "C" fn mvt_renderer_draw_labels(
    handle: *mut MvtRenderer,
    z: u8,
    x: u32,
    y: u32,
    tile_size: u32,
    rgba: *mut u8,
    rgba_len: usize,
    data: *const u8,
    data_len: usize,
    lengths: *const u32,
    lengths_len: usize,
) -> c_int {
    if handle.is_null() {
        return MVT_ERR_NULL_HANDLE;
    }
    if rgba.is_null() || rgba_len == 0 {
        return MVT_ERR_BAD_ARGUMENT;
    }
    let result = catch_unwind(AssertUnwindSafe(|| {
        let Ok(slices) = unpack_tiles(data, data_len, lengths, lengths_len) else {
            return MVT_ERR_BAD_ARGUMENT;
        };
        let pixels = std::slice::from_raw_parts_mut(rgba, rgba_len);
        (*handle)
            .inner
            .draw_labels_onto(z, x, y, tile_size, pixels, &slices)
            .map(|placed| placed as c_int)
            .unwrap_or(MVT_ERR_RENDER_FAILED)
    }));
    result.unwrap_or(MVT_ERR_PANIC)
}

/// Shared shape of the accessors that answer with a list of strings.
///
/// # Safety
/// `handle` must be live.
unsafe fn json_array_from(
    handle: *mut MvtRenderer,
    read: impl Fn(&Renderer) -> Vec<String>,
) -> *mut c_char {
    if handle.is_null() {
        return ptr::null_mut();
    }
    let result = catch_unwind(AssertUnwindSafe(|| {
        let items: Vec<String> = read(&(*handle).inner)
            .iter()
            .map(|item| json_string(item))
            .collect();
        format!("[{}]", items.join(","))
    }));
    match result {
        Ok(json) => into_c_string(json),
        Err(_) => ptr::null_mut(),
    }
}

/// Shared shape of the accessors that answer yes or no.
///
/// # Safety
/// `handle` must be live.
unsafe fn bool_call(handle: *mut MvtRenderer, read: impl Fn(&Renderer) -> bool) -> c_int {
    if handle.is_null() {
        return MVT_ERR_NULL_HANDLE;
    }
    let result = catch_unwind(AssertUnwindSafe(|| c_int::from(read(&(*handle).inner))));
    result.unwrap_or(MVT_ERR_PANIC)
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
