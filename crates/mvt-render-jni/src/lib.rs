//! JNI surface for the vector tile renderer.
//!
//! Mirrors the wasm binding: the host passes every fetched tile concatenated
//! into one buffer plus a length table, and gets PNG bytes back. Nothing calls
//! from Rust into the JVM, so no thread ever needs `AttachCurrentThread` — the
//! one place where JNI overhead genuinely costs something.
//!
//! The renderer is handed to Kotlin as an opaque `long`. Every entry point
//! catches unwinds: this code runs inside someone else's app, so a panic must
//! become a Java exception rather than an abort.

use std::panic::{catch_unwind, AssertUnwindSafe};

#[cfg(feature = "tessellate")]
use jni::objects::JByteBuffer;
use jni::objects::{JByteArray, JClass, JIntArray, JString};
#[cfg(feature = "tessellate")]
use jni::sys::jobject;
use jni::sys::{jbyteArray, jint, jlong, jstring};
use jni::JNIEnv;
use mvt_render::Renderer;

const RUNTIME_EXCEPTION: &str = "java/lang/RuntimeException";
const ILLEGAL_ARGUMENT: &str = "java/lang/IllegalArgumentException";

/// Turns a raw handle back into a renderer reference.
///
/// # Safety
/// `handle` must come from `nativeNew` and not yet have been freed.
unsafe fn renderer<'a>(handle: jlong) -> Option<&'a mut Renderer> {
    if handle == 0 {
        return None;
    }
    Some(&mut *(handle as *mut Renderer))
}

fn throw(env: &mut JNIEnv, class: &str, message: &str) {
    // If throwing itself fails there is nothing useful left to do; the JVM
    // will report the pending exception.
    let _ = env.throw_new(class, message);
}

/// Runs `body`, converting a panic into a Java exception.
fn guarded<T>(env: &mut JNIEnv, fallback: T, body: impl FnOnce(&mut JNIEnv) -> T) -> T {
    match catch_unwind(AssertUnwindSafe(|| body(env))) {
        Ok(value) => value,
        Err(_) => {
            throw(env, RUNTIME_EXCEPTION, "vector tile renderer panicked");
            fallback
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_com_mapconductor_vectortile_NativeRenderer_nativeNew(
    mut env: JNIEnv,
    _class: JClass,
    style_json: JString,
    // dp one tile covers where the host shows it. Not the pixel count a render
    // is asked for: a label pass draws at twice the pixels, and a 256 dp tile
    // is still 256 dp however many pixels it carries. Sets the size the style
    // draws at and the zoom its expressions are read at.
    display_tile_size: jint,
) -> jlong {
    guarded(&mut env, 0, |env| {
        let style: String = match env.get_string(&style_json) {
            Ok(value) => value.into(),
            Err(error) => {
                throw(env, ILLEGAL_ARGUMENT, &format!("bad style string: {error}"));
                return 0;
            }
        };
        match Renderer::new(&style) {
            Ok(renderer) => {
                let renderer = renderer.with_display_tile_size(display_tile_size.max(1) as u32);
                Box::into_raw(Box::new(renderer)) as jlong
            }
            Err(error) => {
                throw(env, ILLEGAL_ARGUMENT, &error.to_string());
                0
            }
        }
    })
}

#[no_mangle]
pub extern "system" fn Java_com_mapconductor_vectortile_NativeRenderer_nativeFree(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
) {
    if handle == 0 {
        return;
    }
    // Reconstituting the Box drops the renderer.
    unsafe { drop(Box::from_raw(handle as *mut Renderer)) };
}

#[no_mangle]
pub extern "system" fn Java_com_mapconductor_vectortile_NativeRenderer_nativeSetStyle(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    style_json: JString,
) {
    guarded(&mut env, (), |env| {
        let Some(renderer) = (unsafe { renderer(handle) }) else {
            throw(env, ILLEGAL_ARGUMENT, "renderer handle is null");
            return;
        };
        let style: String = match env.get_string(&style_json) {
            Ok(value) => value.into(),
            Err(error) => {
                throw(env, ILLEGAL_ARGUMENT, &format!("bad style string: {error}"));
                return;
            }
        };
        if let Err(error) = renderer.set_style(&style) {
            throw(env, ILLEGAL_ARGUMENT, &error.to_string());
        }
    })
}

/// Returns the plan as a JSON array of
/// `{ sourceId, url, z, x, y, scale, offsetX, offsetY }`.
#[no_mangle]
pub extern "system" fn Java_com_mapconductor_vectortile_NativeRenderer_nativePlan(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    z: jint,
    x: jint,
    y: jint,
) -> jstring {
    guarded(&mut env, std::ptr::null_mut(), |env| {
        let Some(renderer) = (unsafe { renderer(handle) }) else {
            throw(env, ILLEGAL_ARGUMENT, "renderer handle is null");
            return std::ptr::null_mut();
        };
        let json = plan_json(renderer, z, x, y);
        match env.new_string(json) {
            Ok(value) => value.into_raw(),
            Err(error) => {
                throw(env, RUNTIME_EXCEPTION, &format!("{error}"));
                std::ptr::null_mut()
            }
        }
    })
}

fn plan_json(renderer: &Renderer, z: jint, x: jint, y: jint) -> String {
    let plan = renderer.plan(z as u8, x as u32, y as u32);
    let mut out = String::from("[");
    for (index, request) in plan.requests.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        // Hand-built rather than pulling serde_json in for six fields; the
        // only interpolated string is a URL from the style.
        out.push_str(&format!(
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
        ));
    }
    out.push(']');
    out
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

/// Rasterises one tile.
///
/// `data` is every fetched source tile concatenated in plan order; `lengths`
/// gives each tile's byte length, with `0` marking one that failed to fetch.
#[no_mangle]
pub extern "system" fn Java_com_mapconductor_vectortile_NativeRenderer_nativeRender(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    z: jint,
    x: jint,
    y: jint,
    tile_size: jint,
    data: JByteArray,
    lengths: JIntArray,
    // Non-zero leaves labels and icons out, for a host that draws them on a
    // separate layer stacked above this one.
    geometry_only: jint,
) -> jbyteArray {
    guarded(&mut env, std::ptr::null_mut(), |env| {
        let Some(renderer) = (unsafe { renderer(handle) }) else {
            throw(env, ILLEGAL_ARGUMENT, "renderer handle is null");
            return std::ptr::null_mut();
        };

        let data_bytes = match env.convert_byte_array(&data) {
            Ok(value) => value,
            Err(error) => {
                throw(env, ILLEGAL_ARGUMENT, &format!("bad tile data: {error}"));
                return std::ptr::null_mut();
            }
        };

        let count = match env.get_array_length(&lengths) {
            Ok(value) => value as usize,
            Err(error) => {
                throw(env, ILLEGAL_ARGUMENT, &format!("bad length table: {error}"));
                return std::ptr::null_mut();
            }
        };
        let mut length_table = vec![0i32; count];
        if let Err(error) = env.get_int_array_region(&lengths, 0, &mut length_table) {
            throw(env, ILLEGAL_ARGUMENT, &format!("bad length table: {error}"));
            return std::ptr::null_mut();
        }

        let mut slices: Vec<Option<&[u8]>> = Vec::with_capacity(count);
        let mut offset = 0usize;
        for length in &length_table {
            if *length <= 0 {
                slices.push(None);
                continue;
            }
            let length = *length as usize;
            let end = match offset.checked_add(length) {
                Some(end) if end <= data_bytes.len() => end,
                _ => {
                    throw(
                        env,
                        ILLEGAL_ARGUMENT,
                        "tile length table overruns the data buffer",
                    );
                    return std::ptr::null_mut();
                }
            };
            slices.push(Some(&data_bytes[offset..end]));
            offset = end;
        }

        let rendered = if geometry_only != 0 {
            renderer.render_geometry(z as u8, x as u32, y as u32, tile_size as u32, &slices)
        } else {
            renderer.render(z as u8, x as u32, y as u32, tile_size as u32, &slices)
        };
        let png = match rendered {
            Ok(png) => png,
            Err(error) => {
                throw(env, RUNTIME_EXCEPTION, &error.to_string());
                return std::ptr::null_mut();
            }
        };

        match env.byte_array_from_slice(&png) {
            Ok(array) => array.into_raw(),
            Err(error) => {
                throw(env, RUNTIME_EXCEPTION, &format!("{error}"));
                std::ptr::null_mut()
            }
        }
    })
}

/// Reasons the current style may not render as intended, as a JSON array of
/// strings.
#[no_mangle]
pub extern "system" fn Java_com_mapconductor_vectortile_NativeRenderer_nativeDiagnostics(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
) -> jstring {
    guarded(&mut env, std::ptr::null_mut(), |env| {
        let Some(renderer) = (unsafe { renderer(handle) }) else {
            throw(env, ILLEGAL_ARGUMENT, "renderer handle is null");
            return std::ptr::null_mut();
        };
        let messages: Vec<String> = renderer
            .diagnostics()
            .iter()
            .map(|message| json_string(message))
            .collect();
        match env.new_string(format!("[{}]", messages.join(","))) {
            Ok(value) => value.into_raw(),
            Err(error) => {
                throw(env, RUNTIME_EXCEPTION, &format!("{error}"));
                std::ptr::null_mut()
            }
        }
    })
}

/// Layer `type` values in the current style that will not be drawn, as a JSON
/// array of strings.
#[no_mangle]
/// The style's `glyphs` URL template, or an empty string when it has none.
pub extern "system" fn Java_com_mapconductor_vectortile_NativeRenderer_nativeGlyphsUrlTemplate(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
) -> jstring {
    guarded(&mut env, std::ptr::null_mut(), |env| {
        let Some(renderer) = (unsafe { renderer(handle) }) else {
            throw(env, ILLEGAL_ARGUMENT, "renderer handle is null");
            return std::ptr::null_mut();
        };
        match env.new_string(renderer.glyphs_url_template().unwrap_or("")) {
            Ok(value) => value.into_raw(),
            Err(error) => {
                throw(env, RUNTIME_EXCEPTION, &format!("{error}"));
                std::ptr::null_mut()
            }
        }
    })
}

#[no_mangle]
/// The `{fontstack}/{range}.pbf` files this tile's labels need and the
/// renderer does not have, as a JSON array of already-filled URLs.
///
/// URLs rather than parts: the host has nothing to do with the pair except
/// fetch it, and building the string here keeps one copy of the substitution
/// rules instead of one per platform.
pub extern "system" fn Java_com_mapconductor_vectortile_NativeRenderer_nativeQueryFeatures(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    z: jint,
    x: jint,
    y: jint,
    data: JByteArray,
    lengths: JIntArray,
    source_layer: JString,
    filter_json: JString,
    text: JString,
    limit: jint,
) -> jstring {
    // Features in the tile as JSON; see `Renderer::query_features_json`.
    // Null strings mean "no restriction". A filter that is not JSON throws.
    guarded(&mut env, std::ptr::null_mut(), |env| {
        let Some(renderer) = (unsafe { renderer(handle) }) else {
            throw(env, ILLEGAL_ARGUMENT, "renderer handle is null");
            return std::ptr::null_mut();
        };
        let data_bytes = match env.convert_byte_array(&data) {
            Ok(value) => value,
            Err(error) => {
                throw(env, ILLEGAL_ARGUMENT, &format!("bad tile data: {error}"));
                return std::ptr::null_mut();
            }
        };
        let count = match env.get_array_length(&lengths) {
            Ok(value) => value as usize,
            Err(error) => {
                throw(env, ILLEGAL_ARGUMENT, &format!("bad length table: {error}"));
                return std::ptr::null_mut();
            }
        };
        let mut length_table = vec![0i32; count];
        if let Err(error) = env.get_int_array_region(&lengths, 0, &mut length_table) {
            throw(env, ILLEGAL_ARGUMENT, &format!("bad length table: {error}"));
            return std::ptr::null_mut();
        }
        let mut slices: Vec<Option<&[u8]>> = Vec::with_capacity(count);
        let mut offset = 0usize;
        for length in &length_table {
            if *length <= 0 {
                slices.push(None);
                continue;
            }
            let length = *length as usize;
            if offset + length > data_bytes.len() {
                slices.push(None);
                continue;
            }
            slices.push(Some(&data_bytes[offset..offset + length]));
            offset += length;
        }
        let source_layer = optional_string(env, &source_layer);
        let filter_json = optional_string(env, &filter_json);
        let text = optional_string(env, &text);
        let json = match renderer.query_features_json(
            z as u8,
            x as u32,
            y as u32,
            &slices,
            source_layer.as_deref(),
            filter_json.as_deref(),
            text.as_deref(),
            limit.max(0) as usize,
        ) {
            Ok(json) => json,
            Err(error) => {
                throw(env, ILLEGAL_ARGUMENT, &format!("{error}"));
                return std::ptr::null_mut();
            }
        };
        match env.new_string(json) {
            Ok(value) => value.into_raw(),
            Err(error) => {
                throw(env, RUNTIME_EXCEPTION, &format!("{error}"));
                std::ptr::null_mut()
            }
        }
    })
}

/// A possibly-null Java string.
fn optional_string(env: &mut JNIEnv, value: &JString) -> Option<String> {
    if value.is_null() {
        return None;
    }
    env.get_string(value).ok().map(|s| s.into())
}

#[no_mangle]
pub extern "system" fn Java_com_mapconductor_vectortile_NativeRenderer_nativeNeededGlyphs(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    z: jint,
    x: jint,
    y: jint,
    data: JByteArray,
    lengths: JIntArray,
) -> jstring {
    guarded(&mut env, std::ptr::null_mut(), |env| {
        let Some(renderer) = (unsafe { renderer(handle) }) else {
            throw(env, ILLEGAL_ARGUMENT, "renderer handle is null");
            return std::ptr::null_mut();
        };
        let Some(template) = renderer.glyphs_url_template().map(str::to_owned) else {
            return empty_json_array(env);
        };

        let data_bytes = match env.convert_byte_array(&data) {
            Ok(value) => value,
            Err(error) => {
                throw(env, ILLEGAL_ARGUMENT, &format!("bad tile data: {error}"));
                return std::ptr::null_mut();
            }
        };
        let count = match env.get_array_length(&lengths) {
            Ok(value) => value as usize,
            Err(error) => {
                throw(env, ILLEGAL_ARGUMENT, &format!("bad length table: {error}"));
                return std::ptr::null_mut();
            }
        };
        let mut length_table = vec![0i32; count];
        if let Err(error) = env.get_int_array_region(&lengths, 0, &mut length_table) {
            throw(env, ILLEGAL_ARGUMENT, &format!("bad length table: {error}"));
            return std::ptr::null_mut();
        }

        let mut slices: Vec<Option<&[u8]>> = Vec::with_capacity(count);
        let mut offset = 0usize;
        for length in &length_table {
            if *length <= 0 {
                slices.push(None);
                continue;
            }
            let length = *length as usize;
            if offset + length > data_bytes.len() {
                slices.push(None);
                continue;
            }
            slices.push(Some(&data_bytes[offset..offset + length]));
            offset += length;
        }

        let urls: Vec<String> = renderer
            .needed_glyphs(z as u8, x as u32, y as u32, &slices)
            .iter()
            .map(|request| json_string(&request.url(&template)))
            .collect();
        match env.new_string(format!("[{}]", urls.join(","))) {
            Ok(value) => value.into_raw(),
            Err(error) => {
                throw(env, RUNTIME_EXCEPTION, &format!("{error}"));
                std::ptr::null_mut()
            }
        }
    })
}

#[no_mangle]
/// Hands one fetched glyph range to the renderer, which keeps it for every
/// later tile. Returns how many glyphs it gained.
pub extern "system" fn Java_com_mapconductor_vectortile_NativeRenderer_nativeAddGlyphs(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    pbf: JByteArray,
) -> jint {
    guarded(&mut env, 0, |env| {
        let Some(renderer) = (unsafe { renderer(handle) }) else {
            throw(env, ILLEGAL_ARGUMENT, "renderer handle is null");
            return 0;
        };
        let bytes = match env.convert_byte_array(&pbf) {
            Ok(value) => value,
            Err(error) => {
                throw(env, ILLEGAL_ARGUMENT, &format!("bad glyph data: {error}"));
                return 0;
            }
        };
        match renderer.add_glyphs(&bytes) {
            Ok(added) => added as jint,
            Err(error) => {
                // A bad range is not fatal: the label simply misses those
                // characters, and the rest of the tile is unaffected.
                throw(env, ILLEGAL_ARGUMENT, &format!("{error}"));
                0
            }
        }
    })
}

#[no_mangle]
/// The style's sprite pair for this pixel ratio, as a JSON array of two URLs
/// (index first, then image), or an empty array when the style has no sprite.
pub extern "system" fn Java_com_mapconductor_vectortile_NativeRenderer_nativeSpriteUrls(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    pixel_ratio: jint,
) -> jstring {
    guarded(&mut env, std::ptr::null_mut(), |env| {
        let Some(renderer) = (unsafe { renderer(handle) }) else {
            throw(env, ILLEGAL_ARGUMENT, "renderer handle is null");
            return std::ptr::null_mut();
        };
        let urls: Vec<String> = match renderer.sprite_urls(pixel_ratio.max(1) as u32) {
            Some(request) => vec![
                json_string(&request.json_url),
                json_string(&request.png_url),
            ],
            None => Vec::new(),
        };
        match env.new_string(format!("[{}]", urls.join(","))) {
            Ok(value) => value.into_raw(),
            Err(error) => {
                throw(env, RUNTIME_EXCEPTION, &format!("{error}"));
                std::ptr::null_mut()
            }
        }
    })
}

#[no_mangle]
/// Whether a tile at this zoom has to be drawn on the CPU, because the style
/// paints something the GPU path cannot -- today, a patterned fill.
pub extern "system" fn Java_com_mapconductor_vectortile_NativeRenderer_nativeNeedsCpu(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    z: jint,
    data: JByteArray,
    lengths: JIntArray,
) -> jint {
    guarded(&mut env, 0, |env| {
        let Some(renderer) = (unsafe { renderer(handle) }) else {
            throw(env, ILLEGAL_ARGUMENT, "renderer handle is null");
            return 0;
        };
        let data_bytes = match env.convert_byte_array(&data) {
            Ok(value) => value,
            Err(error) => {
                throw(env, ILLEGAL_ARGUMENT, &format!("bad tile data: {error}"));
                return 0;
            }
        };
        let count = match env.get_array_length(&lengths) {
            Ok(value) => value as usize,
            Err(error) => {
                throw(env, ILLEGAL_ARGUMENT, &format!("bad length table: {error}"));
                return 0;
            }
        };
        let mut length_table = vec![0i32; count];
        if let Err(error) = env.get_int_array_region(&lengths, 0, &mut length_table) {
            throw(env, ILLEGAL_ARGUMENT, &format!("bad length table: {error}"));
            return 0;
        }

        let mut slices: Vec<Option<&[u8]>> = Vec::with_capacity(count);
        let mut offset = 0usize;
        for length in &length_table {
            if *length <= 0 {
                slices.push(None);
                continue;
            }
            let length = *length as usize;
            if offset + length > data_bytes.len() {
                slices.push(None);
                continue;
            }
            slices.push(Some(&data_bytes[offset..offset + length]));
            offset += length;
        }

        jint::from(renderer.needs_cpu(z.clamp(0, 30) as u8, &slices))
    })
}

#[no_mangle]
/// Whether the style names a sprite the renderer has not been given yet.
pub extern "system" fn Java_com_mapconductor_vectortile_NativeRenderer_nativeNeedsSprite(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
) -> jint {
    guarded(&mut env, 0, |env| {
        let Some(renderer) = (unsafe { renderer(handle) }) else {
            throw(env, ILLEGAL_ARGUMENT, "renderer handle is null");
            return 0;
        };
        jint::from(renderer.needs_sprite())
    })
}

#[no_mangle]
/// Takes the fetched sprite pair and keeps the sheet. Returns the icon count.
pub extern "system" fn Java_com_mapconductor_vectortile_NativeRenderer_nativeAddSprite(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    index_json: JString,
    png: JByteArray,
) -> jint {
    guarded(&mut env, 0, |env| {
        let Some(renderer) = (unsafe { renderer(handle) }) else {
            throw(env, ILLEGAL_ARGUMENT, "renderer handle is null");
            return 0;
        };
        let json: String = match env.get_string(&index_json) {
            Ok(value) => value.into(),
            Err(error) => {
                throw(env, ILLEGAL_ARGUMENT, &format!("bad sprite index: {error}"));
                return 0;
            }
        };
        let bytes = match env.convert_byte_array(&png) {
            Ok(value) => value,
            Err(error) => {
                throw(env, ILLEGAL_ARGUMENT, &format!("bad sprite sheet: {error}"));
                return 0;
            }
        };
        match renderer.add_sprite(&json, &bytes) {
            Ok(icons) => icons as jint,
            Err(error) => {
                // A broken sheet costs the icons and nothing else; the rest of
                // the tile, labels included, still draws.
                throw(env, ILLEGAL_ARGUMENT, &format!("{error}"));
                0
            }
        }
    })
}

#[no_mangle]
/// Draws this tile's labels onto an RGBA buffer the GPU produced, in place.
///
/// Returns how many labels were placed, or 0 when there is nothing to draw.
pub extern "system" fn Java_com_mapconductor_vectortile_NativeRenderer_nativeDrawLabels(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    z: jint,
    x: jint,
    y: jint,
    tile_size: jint,
    rgba: JByteArray,
    data: JByteArray,
    lengths: JIntArray,
) -> jint {
    guarded(&mut env, 0, |env| {
        let Some(renderer) = (unsafe { renderer(handle) }) else {
            throw(env, ILLEGAL_ARGUMENT, "renderer handle is null");
            return 0;
        };

        let mut pixels = match env.convert_byte_array(&rgba) {
            Ok(value) => value,
            Err(error) => {
                throw(env, ILLEGAL_ARGUMENT, &format!("bad rgba buffer: {error}"));
                return 0;
            }
        };
        let data_bytes = match env.convert_byte_array(&data) {
            Ok(value) => value,
            Err(error) => {
                throw(env, ILLEGAL_ARGUMENT, &format!("bad tile data: {error}"));
                return 0;
            }
        };
        let count = match env.get_array_length(&lengths) {
            Ok(value) => value as usize,
            Err(error) => {
                throw(env, ILLEGAL_ARGUMENT, &format!("bad length table: {error}"));
                return 0;
            }
        };
        let mut length_table = vec![0i32; count];
        if let Err(error) = env.get_int_array_region(&lengths, 0, &mut length_table) {
            throw(env, ILLEGAL_ARGUMENT, &format!("bad length table: {error}"));
            return 0;
        }
        let mut slices: Vec<Option<&[u8]>> = Vec::with_capacity(count);
        let mut offset = 0usize;
        for length in &length_table {
            if *length <= 0 {
                slices.push(None);
                continue;
            }
            let length = *length as usize;
            if offset + length > data_bytes.len() {
                slices.push(None);
                continue;
            }
            slices.push(Some(&data_bytes[offset..offset + length]));
            offset += length;
        }

        let placed = match renderer.draw_labels_onto(
            z as u8,
            x as u32,
            y as u32,
            tile_size as u32,
            &mut pixels,
            &slices,
        ) {
            Ok(count) => count,
            Err(error) => {
                throw(env, ILLEGAL_ARGUMENT, &format!("{error}"));
                return 0;
            }
        };
        if placed > 0 {
            // Written back only when something changed: the copy is the whole
            // buffer, and most tiles at most zooms carry no label at all.
            if let Err(error) = env.set_byte_array_region(&rgba, 0, unsafe {
                std::slice::from_raw_parts(pixels.as_ptr() as *const i8, pixels.len())
            }) {
                throw(env, RUNTIME_EXCEPTION, &format!("{error}"));
                return 0;
            }
        }
        placed as jint
    })
}

/// Draws a transparent label tile and returns its premultiplied RGBA allocation
/// as a direct buffer. The label count is written to `placed_out[0]`.
#[cfg(feature = "tessellate")]
#[no_mangle]
pub extern "system" fn Java_com_mapconductor_vectortile_NativeRenderer_nativeRenderLabels(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    z: jint,
    x: jint,
    y: jint,
    tile_size: jint,
    data: JByteArray,
    lengths: JIntArray,
    placed_out: JIntArray,
) -> jobject {
    guarded(&mut env, std::ptr::null_mut(), |env| {
        let Some(renderer) = (unsafe { renderer(handle) }) else {
            throw(env, ILLEGAL_ARGUMENT, "renderer handle is null");
            return std::ptr::null_mut();
        };
        if env.get_array_length(&placed_out).unwrap_or(0) < 1 {
            throw(env, ILLEGAL_ARGUMENT, "placed output is empty");
            return std::ptr::null_mut();
        }

        let data_bytes = match env.convert_byte_array(&data) {
            Ok(value) => value,
            Err(error) => {
                throw(env, ILLEGAL_ARGUMENT, &format!("bad tile data: {error}"));
                return std::ptr::null_mut();
            }
        };
        let count = match env.get_array_length(&lengths) {
            Ok(value) => value as usize,
            Err(error) => {
                throw(env, ILLEGAL_ARGUMENT, &format!("bad length table: {error}"));
                return std::ptr::null_mut();
            }
        };
        let mut length_table = vec![0i32; count];
        if let Err(error) = env.get_int_array_region(&lengths, 0, &mut length_table) {
            throw(env, ILLEGAL_ARGUMENT, &format!("bad length table: {error}"));
            return std::ptr::null_mut();
        }
        let mut slices: Vec<Option<&[u8]>> = Vec::with_capacity(count);
        let mut offset = 0usize;
        for length in &length_table {
            if *length <= 0 {
                slices.push(None);
                continue;
            }
            let length = *length as usize;
            if offset + length > data_bytes.len() {
                slices.push(None);
                continue;
            }
            slices.push(Some(&data_bytes[offset..offset + length]));
            offset += length;
        }

        let (pixels, placed) = match renderer.render_labels_rgba(
            z as u8,
            x as u32,
            y as u32,
            tile_size as u32,
            &slices,
        ) {
            Ok(result) => result,
            Err(error) => {
                throw(env, ILLEGAL_ARGUMENT, &format!("{error}"));
                return std::ptr::null_mut();
            }
        };
        if env
            .set_int_array_region(&placed_out, 0, &[placed as jint])
            .is_err()
        {
            throw(env, RUNTIME_EXCEPTION, "could not return label count");
            return std::ptr::null_mut();
        }
        if pixels.is_empty() {
            return std::ptr::null_mut();
        }

        let mut pixels = pixels.into_boxed_slice();
        let byte_len = pixels.len();
        let address = pixels.as_mut_ptr();
        match unsafe { env.new_direct_byte_buffer(address, byte_len) } {
            Ok(buffer) => {
                std::mem::forget(pixels);
                buffer.into_raw()
            }
            Err(error) => {
                throw(env, RUNTIME_EXCEPTION, &format!("{error}"));
                std::ptr::null_mut()
            }
        }
    })
}

/// Releases the RGBA allocation returned by `nativeRenderLabels`.
#[cfg(feature = "tessellate")]
#[no_mangle]
pub extern "system" fn Java_com_mapconductor_vectortile_NativeRenderer_nativeFreeLabelPixels(
    mut env: JNIEnv,
    _class: JClass,
    buffer: JByteBuffer,
) {
    guarded(&mut env, (), |env| {
        let address = match env.get_direct_buffer_address(&buffer) {
            Ok(address) => address,
            Err(error) => {
                throw(
                    env,
                    ILLEGAL_ARGUMENT,
                    &format!("not a direct buffer: {error}"),
                );
                return;
            }
        };
        let byte_len = match env.get_direct_buffer_capacity(&buffer) {
            Ok(byte_len) => byte_len,
            Err(error) => {
                throw(
                    env,
                    ILLEGAL_ARGUMENT,
                    &format!("bad direct buffer: {error}"),
                );
                return;
            }
        };
        let slice = std::ptr::slice_from_raw_parts_mut(address, byte_len);
        drop(unsafe { Box::from_raw(slice) });
    });
}

fn empty_json_array(env: &mut JNIEnv) -> jstring {
    match env.new_string("[]") {
        Ok(value) => value.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// JSON array of the credits the style's sources ask to be shown. May contain
/// HTML: the host's attribution overlay renders it.
#[no_mangle]
pub extern "system" fn Java_com_mapconductor_vectortile_NativeRenderer_nativeAttributions(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
) -> jstring {
    guarded(&mut env, std::ptr::null_mut(), |env| {
        let Some(renderer) = (unsafe { renderer(handle) }) else {
            throw(env, ILLEGAL_ARGUMENT, "renderer handle is null");
            return std::ptr::null_mut();
        };
        let credits: Vec<String> = renderer
            .attributions()
            .iter()
            .map(|credit| json_string(credit))
            .collect();
        match env.new_string(format!("[{}]", credits.join(","))) {
            Ok(value) => value.into_raw(),
            Err(error) => {
                throw(env, RUNTIME_EXCEPTION, &format!("{error}"));
                std::ptr::null_mut()
            }
        }
    })
}

#[no_mangle]
pub extern "system" fn Java_com_mapconductor_vectortile_NativeRenderer_nativeUnsupportedLayerTypes(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
) -> jstring {
    guarded(&mut env, std::ptr::null_mut(), |env| {
        let Some(renderer) = (unsafe { renderer(handle) }) else {
            throw(env, ILLEGAL_ARGUMENT, "renderer handle is null");
            return std::ptr::null_mut();
        };
        let kinds: Vec<String> = renderer
            .unsupported_layer_types()
            .iter()
            .map(|kind| json_string(kind))
            .collect();
        match env.new_string(format!("[{}]", kinds.join(","))) {
            Ok(value) => value.into_raw(),
            Err(error) => {
                throw(env, RUNTIME_EXCEPTION, &format!("{error}"));
                std::ptr::null_mut()
            }
        }
    })
}

/// Triangulates a tile for a GPU, returning one packed direct byte buffer.
///
/// Layout, chosen so a single JNI call carries everything with no object
/// marshalling:
///
/// ```text
/// [0]                       extent
/// [1]                       background r,g,b,a present (1) or not (0)
/// [2..6]                    background colour
/// [6]                       batch count B
/// [7..13]                   timings ms: copy-in, decode, tessellate,
///                           filter-compile, fill, line
/// [13 .. 13+B*2]            per batch: firstVertex, vertexCount
/// [13+B*2 ..]               vertices: x, y, r, g, b, a
/// ```
///
/// The timings exist because the device-side cost had to be decomposed
/// somewhere, and the only clock that can separate the JNI copy from the
/// decode from the tessellation is the one running inside this function.
///
/// Indices are exact in f32 up to 2^24, far beyond any tile's vertex count.
#[cfg(feature = "tessellate")]
#[no_mangle]
pub extern "system" fn Java_com_mapconductor_vectortile_NativeRenderer_nativeTessellate(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    z: jint,
    x: jint,
    y: jint,
    tile_size: jint,
    data: JByteArray,
    lengths: JIntArray,
) -> jobject {
    guarded(&mut env, std::ptr::null_mut(), |env| {
        let Some(renderer) = (unsafe { renderer(handle) }) else {
            throw(env, ILLEGAL_ARGUMENT, "renderer handle is null");
            return std::ptr::null_mut();
        };

        let Ok(data_bytes) = env.convert_byte_array(&data) else {
            throw(env, ILLEGAL_ARGUMENT, "bad tile data");
            return std::ptr::null_mut();
        };
        let Ok(count) = env.get_array_length(&lengths) else {
            throw(env, ILLEGAL_ARGUMENT, "bad length table");
            return std::ptr::null_mut();
        };
        let mut table = vec![0i32; count as usize];
        if env.get_int_array_region(&lengths, 0, &mut table).is_err() {
            throw(env, ILLEGAL_ARGUMENT, "bad length table");
            return std::ptr::null_mut();
        }

        let mut slices: Vec<Option<&[u8]>> = Vec::with_capacity(table.len());
        let mut offset = 0usize;
        for length in &table {
            if *length <= 0 {
                slices.push(None);
                continue;
            }
            let end = offset + *length as usize;
            if end > data_bytes.len() {
                throw(
                    env,
                    ILLEGAL_ARGUMENT,
                    "length table overruns the data buffer",
                );
                return std::ptr::null_mut();
            }
            slices.push(Some(&data_bytes[offset..end]));
            offset = end;
        }

        // Packing lives in the renderer, so this and the C ABI cannot drift
        // apart on a layout both sides hard-code offsets into.
        let packed = match renderer.tessellate_packed(
            z as u8,
            x as u32,
            y as u32,
            tile_size as u32,
            &slices,
        ) {
            Ok(packed) => packed,
            Err(error) => {
                throw(env, RUNTIME_EXCEPTION, &format!("{error}"));
                return std::ptr::null_mut();
            }
        };

        // Give Java a view of this allocation instead of copying the packed
        // floats into a Java array. Kotlin releases it after glBufferData has
        // synchronously consumed the client data.
        let mut packed = packed.into_boxed_slice();
        let byte_len = std::mem::size_of_val(packed.as_ref());
        let address = packed.as_mut_ptr().cast::<u8>();
        match unsafe { env.new_direct_byte_buffer(address, byte_len) } {
            Ok(buffer) => {
                std::mem::forget(packed);
                buffer.into_raw()
            }
            Err(error) => {
                throw(env, RUNTIME_EXCEPTION, &format!("{error}"));
                std::ptr::null_mut()
            }
        }
    })
}

/// Releases a packed tessellation returned by `nativeTessellate`.
///
/// DirectByteBuffer does not own its native address, so the caller must invoke
/// this exactly once after OpenGL has consumed the data.
#[cfg(feature = "tessellate")]
#[no_mangle]
pub extern "system" fn Java_com_mapconductor_vectortile_NativeRenderer_nativeFreeTessellation(
    mut env: JNIEnv,
    _class: JClass,
    buffer: JByteBuffer,
) {
    guarded(&mut env, (), |env| {
        let address = match env.get_direct_buffer_address(&buffer) {
            Ok(address) => address,
            Err(error) => {
                throw(
                    env,
                    ILLEGAL_ARGUMENT,
                    &format!("not a direct buffer: {error}"),
                );
                return;
            }
        };
        let byte_len = match env.get_direct_buffer_capacity(&buffer) {
            Ok(byte_len) => byte_len,
            Err(error) => {
                throw(
                    env,
                    ILLEGAL_ARGUMENT,
                    &format!("bad direct buffer: {error}"),
                );
                return;
            }
        };
        if byte_len % std::mem::size_of::<f32>() != 0 {
            throw(
                env,
                ILLEGAL_ARGUMENT,
                "tessellation buffer has invalid size",
            );
            return;
        }

        let float_len = byte_len / std::mem::size_of::<f32>();
        let slice = std::ptr::slice_from_raw_parts_mut(address.cast::<f32>(), float_len);
        drop(unsafe { Box::from_raw(slice) });
    });
}

#[cfg(feature = "glbench-compat")]
#[no_mangle]
pub extern "system" fn Java_com_mapconductor_glbench_NativeTessellator_nativeNew(
    env: JNIEnv,
    class: JClass,
    style_json: JString,
) -> jlong {
    Java_com_mapconductor_vectortile_NativeRenderer_nativeNew(env, class, style_json)
}

#[cfg(feature = "glbench-compat")]
#[no_mangle]
pub extern "system" fn Java_com_mapconductor_glbench_NativeTessellator_nativeFree(
    env: JNIEnv,
    class: JClass,
    handle: jlong,
) {
    Java_com_mapconductor_vectortile_NativeRenderer_nativeFree(env, class, handle)
}

#[cfg(feature = "glbench-compat")]
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_com_mapconductor_glbench_NativeTessellator_nativeRender(
    env: JNIEnv,
    class: JClass,
    handle: jlong,
    z: jint,
    x: jint,
    y: jint,
    tile_size: jint,
    data: JByteArray,
    lengths: JIntArray,
) -> jbyteArray {
    Java_com_mapconductor_vectortile_NativeRenderer_nativeRender(
        env, class, handle, z, x, y, tile_size, data, lengths, 0,
    )
}

#[cfg(feature = "glbench-compat")]
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_com_mapconductor_glbench_NativeTessellator_nativeTessellate(
    env: JNIEnv,
    class: JClass,
    handle: jlong,
    z: jint,
    x: jint,
    y: jint,
    tile_size: jint,
    data: JByteArray,
    lengths: JIntArray,
) -> jobject {
    Java_com_mapconductor_vectortile_NativeRenderer_nativeTessellate(
        env, class, handle, z, x, y, tile_size, data, lengths,
    )
}
