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

use jni::objects::{JByteArray, JClass, JIntArray, JString};
#[cfg(feature = "tessellate")]
use jni::objects::JByteBuffer;
use jni::sys::{jbyteArray, jint, jlong, jstring};
#[cfg(feature = "tessellate")]
use jni::sys::jfloatArray;
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
            Ok(renderer) => Box::into_raw(Box::new(renderer)) as jlong,
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
            r#"{{"sourceId":{},"url":{},"z":{},"x":{},"y":{},"scale":{},"offsetX":{},"offsetY":{}}}"#,
            json_string(&request.source_id),
            json_string(&request.url),
            request.z,
            request.x,
            request.y,
            request.scale,
            request.offset_x,
            request.offset_y,
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

        let png = match renderer.render(z as u8, x as u32, y as u32, tile_size as u32, &slices) {
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

/// Triangulates a tile for a GPU, returning one packed `float[]`.
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
/// [10 .. 10+B*6]            per batch: r, g, b, a, firstVertex, vertexCount
/// [10+B*6 ..]               interleaved x, y vertex positions
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
) -> jfloatArray {
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
                throw(env, ILLEGAL_ARGUMENT, "length table overruns the data buffer");
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

        match env.new_float_array(packed.len() as i32) {
            Ok(array) => {
                if env.set_float_array_region(&array, 0, &packed).is_err() {
                    throw(env, RUNTIME_EXCEPTION, "could not fill the result array");
                    return std::ptr::null_mut();
                }
                array.into_raw()
            }
            Err(error) => {
                throw(env, RUNTIME_EXCEPTION, &format!("{error}"));
                std::ptr::null_mut()
            }
        }
    })
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
        env, class, handle, z, x, y, tile_size, data, lengths,
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
) -> jni::sys::jfloatArray {
    Java_com_mapconductor_vectortile_NativeRenderer_nativeTessellate(
        env, class, handle, z, x, y, tile_size, data, lengths,
    )
}

