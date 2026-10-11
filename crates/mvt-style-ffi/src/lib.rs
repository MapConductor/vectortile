//! C ABI bindings for `mvt-style`, for `MapConductorVectorStyle`.
//!
//! The C twin of `mvt-style-jni`, and the same two calls: JSON in, JSON out,
//! no state between them. Failures arrive inside the JSON as
//! `{"error": "..."}` rather than as a null pointer, so Swift branches on
//! the same thing Kotlin and TypeScript do.
//!
//! A returned pointer owns its bytes and is released with
//! [`mvt_style_string_free`]. Panics never cross the boundary — unwinding
//! into Swift is undefined behaviour — so a panic comes back as null.

use std::ffi::{c_char, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;

/// Reads a C string, or `None` when it is null or not UTF-8.
///
/// # Safety
/// `text` must be null or a valid NUL-terminated string.
unsafe fn read(text: *const c_char) -> Option<String> {
    if text.is_null() {
        return None;
    }
    CStr::from_ptr(text).to_str().ok().map(str::to_owned)
}

fn give(text: String) -> *mut c_char {
    match CString::new(text) {
        Ok(owned) => owned.into_raw(),
        // Only if the JSON contained a NUL byte, which it cannot.
        Err(_) => ptr::null_mut(),
    }
}

/// Applies a rules document to a style document.
///
/// Returns JSON: the adjusted style, the mutations, what they affect, and
/// the diagnostics — or `{"error": "..."}`. Null only when an argument
/// could not be read or the compiler panicked.
///
/// # Safety
/// Both arguments must be valid NUL-terminated UTF-8. The result must be
/// released with [`mvt_style_string_free`].
#[no_mangle]
pub unsafe extern "C" fn mvt_style_compile(
    style_json: *const c_char,
    rules_json: *const c_char,
) -> *mut c_char {
    let result = catch_unwind(AssertUnwindSafe(|| {
        let style = read(style_json)?;
        let rules = read(rules_json)?;
        Some(mvt_style::compile_json(&style, &rules))
    }));
    match result {
        Ok(Some(text)) => give(text),
        _ => ptr::null_mut(),
    }
}

/// The style's layers and the role each would be given, as a JSON array.
///
/// # Safety
/// As [`mvt_style_compile`].
#[no_mangle]
pub unsafe extern "C" fn mvt_style_describe(style_json: *const c_char) -> *mut c_char {
    let result = catch_unwind(AssertUnwindSafe(|| {
        let style = read(style_json)?;
        Some(mvt_style::describe_json(&style))
    }));
    match result {
        Ok(Some(text)) => give(text),
        _ => ptr::null_mut(),
    }
}

/// The rules document version this library understands.
#[no_mangle]
pub extern "C" fn mvt_style_schema_version() -> u32 {
    mvt_style::SCHEMA_VERSION
}

/// Releases a string this library returned.
///
/// # Safety
/// `text` must be null or a pointer this library returned and has not
/// already freed.
#[no_mangle]
pub unsafe extern "C" fn mvt_style_string_free(text: *mut c_char) {
    if !text.is_null() {
        drop(CString::from_raw(text));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trips a string through the C boundary the way Swift will.
    unsafe fn call(f: unsafe extern "C" fn(*const c_char) -> *mut c_char, text: &str) -> String {
        let owned = CString::new(text).expect("no NUL in a test fixture");
        let out = f(owned.as_ptr());
        assert!(!out.is_null(), "the boundary answered with nothing");
        let answer = CStr::from_ptr(out).to_str().expect("UTF-8").to_owned();
        mvt_style_string_free(out);
        answer
    }

    #[test]
    fn compiles_across_the_boundary() {
        let style = CString::new(
            r##"{"version":8,"sources":{},"layers":[
            {"id":"roads","type":"line","source":"s","source-layer":"transportation",
             "paint":{"line-color":"#888"}}]}"##,
        )
        .unwrap();
        let rules =
            CString::new(r##"{"schemaVersion":1,"rules":[{"selector":{"role":"road"},"patch":{"color":"#fff"}}]}"##)
                .unwrap();
        let out = unsafe {
            let raw = mvt_style_compile(style.as_ptr(), rules.as_ptr());
            assert!(!raw.is_null());
            let text = CStr::from_ptr(raw).to_str().unwrap().to_owned();
            mvt_style_string_free(raw);
            text
        };
        assert!(out.contains("\"line-color\""), "{out}");
        assert!(!out.contains("\"error\""), "{out}");
    }

    /// A failure arrives inside the JSON, not as a null pointer: Swift
    /// branches on the same thing Kotlin and TypeScript do.
    #[test]
    fn a_failure_is_json_not_a_null_pointer() {
        let rules = CString::new(r##"{"schemaVersion":1,"rules":[]}"##).unwrap();
        let broken = CString::new("{ not json").unwrap();
        let out = unsafe {
            let raw = mvt_style_compile(broken.as_ptr(), rules.as_ptr());
            assert!(!raw.is_null(), "a bad document is not a bad pointer");
            let text = CStr::from_ptr(raw).to_str().unwrap().to_owned();
            mvt_style_string_free(raw);
            text
        };
        assert!(out.contains("\"error\""), "{out}");
    }

    #[test]
    fn describes_across_the_boundary() {
        let out = unsafe {
            call(
                mvt_style_describe,
                r##"{"version":8,"sources":{},"layers":[{"id":"w","type":"fill","source":"s","source-layer":"water"}]}"##,
            )
        };
        assert!(out.contains("\"water\""), "{out}");
    }

    /// Null in must not be a crash: the Swift side can hand over a nil
    /// string if a document failed to load.
    #[test]
    fn a_null_argument_answers_null_rather_than_crashing() {
        unsafe {
            assert!(mvt_style_compile(std::ptr::null(), std::ptr::null()).is_null());
            assert!(mvt_style_describe(std::ptr::null()).is_null());
            // Freeing nothing is allowed, so a caller need not branch.
            mvt_style_string_free(std::ptr::null_mut());
        }
    }

    #[test]
    fn reports_the_rules_version_the_compiler_was_built_with() {
        assert_eq!(mvt_style_schema_version(), mvt_style::SCHEMA_VERSION);
    }
}
