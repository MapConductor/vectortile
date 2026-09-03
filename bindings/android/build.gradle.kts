plugins {
    // AGP 9 has Kotlin support built in; a separate kotlin.android plugin is
    // rejected. Version matches android-sdk so this module can move into that
    // build later without a toolchain jump.
    id("com.android.library") version "9.2.1" apply false
}
