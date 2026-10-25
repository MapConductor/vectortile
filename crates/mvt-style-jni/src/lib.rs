//! JNI bindings for `mvt-style`.
//!
//! Two calls, both JSON in and JSON out. The whole surface is here because
//! the compiler holds no state: a style and a rule set go in, an adjusted
//! style and the deltas come out, and nothing is kept between calls. That is
//! what lets the Kotlin side be a plain object rather than something with a
//! handle to free.
//!
//! Catches unwinds: this runs inside someone else's app, so a panic must
//! become a Java exception rather than an abort.

use std::panic::{catch_unwind, AssertUnwindSafe};

use jni::objects::{JClass, JString};
use jni::sys::{jint, jstring};
use jni::JNIEnv;

const RUNTIME_EXCEPTION: &str = "java/lang/RuntimeException";

/// Runs `body`, converting a panic into a Java exception and an empty string.
fn guarded(env: &mut JNIEnv, body: impl FnOnce(&mut JNIEnv) -> Option<String>) -> jstring {
    let answer = match catch_unwind(AssertUnwindSafe(|| body(env))) {
        Ok(Some(text)) => text,
        Ok(None) => {
            // A string argument that could not be read. Already an error in
            // JNI terms, so there is nothing useful to return.
            return std::ptr::null_mut();
        }
        Err(_) => {
            let _ = env.throw_new(RUNTIME_EXCEPTION, "vector style compiler panicked");
            return std::ptr::null_mut();
        }
    };
    match env.new_string(answer) {
        Ok(text) => text.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// The compiled result as JSON, or `{"error": "..."}`.
#[no_mangle]
pub extern "system" fn Java_com_mapconductor_vectorstyle_NativeStyleCompiler_nativeCompile(
    mut env: JNIEnv,
    _class: JClass,
    style_json: JString,
    rules_json: JString,
) -> jstring {
    guarded(&mut env, |env| {
        let style: String = env.get_string(&style_json).ok()?.into();
        let rules: String = env.get_string(&rules_json).ok()?.into();
        Some(mvt_style::compile_json(&style, &rules))
    })
}

/// The style's layers and the role each would be given, as JSON.
#[no_mangle]
pub extern "system" fn Java_com_mapconductor_vectorstyle_NativeStyleCompiler_nativeDescribe(
    mut env: JNIEnv,
    _class: JClass,
    style_json: JString,
) -> jstring {
    guarded(&mut env, |env| {
        let style: String = env.get_string(&style_json).ok()?.into();
        Some(mvt_style::describe_json(&style))
    })
}

/// The rules document version this library understands.
///
/// Read by the Kotlin side when it writes a rules document, so the two can
/// never disagree about the shape they are exchanging.
#[no_mangle]
pub extern "system" fn Java_com_mapconductor_vectorstyle_NativeStyleCompiler_nativeSchemaVersion(
    _env: JNIEnv,
    _class: JClass,
) -> jint {
    mvt_style::SCHEMA_VERSION as jint
}
