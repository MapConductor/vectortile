// PNG encoding for the iOS SDK core. Mirrors crates/tile-png-ffi.
//
// Travels as a source header in ios-sdk-core rather than inside the
// XCFramework: Xcode merges every linked framework's Headers into one include
// directory, and two frameworks shipping a module.modulemap collide there.
#ifndef TILE_PNG_H
#define TILE_PNG_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

// Swift's C importer keeps a bare 0 but drops macros whose body starts with a
// minus, so the error codes are restated on the Swift side. Keep them in step.
#define TILE_PNG_OK 0
#define TILE_PNG_ERR_BAD_ARGUMENT (-1)
#define TILE_PNG_ERR_ENCODE_FAILED (-2)
#define TILE_PNG_ERR_PANIC (-3)

/// Encodes an RGBA8 buffer as PNG. Release the result with
/// tile_png_buffer_free. When `premultiplied` is non-zero the input buffer is
/// un-premultiplied in place and must not be reused.
int32_t tile_png_encode(uint8_t *rgba,
                        uint32_t width,
                        uint32_t height,
                        int32_t premultiplied,
                        uint8_t **out_ptr,
                        size_t *out_len);

/// Releases a buffer returned by tile_png_encode.
void tile_png_buffer_free(uint8_t *ptr, size_t len);

#ifdef __cplusplus
}
#endif

#endif /* TILE_PNG_H */
