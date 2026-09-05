// The PNG encoder's C interface, so the bench can time it against the
// platform's on real hardware. The SDK core keeps this internal; a benchmark
// app is not a reason to widen a public API, so it reaches the C symbols
// directly instead.
#import "tile_png.h"
