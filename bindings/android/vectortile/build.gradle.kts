plugins {
    id("com.android.library")
}

android {
    namespace = "com.mapconductor.vectortile"
    compileSdk = 37

    defaultConfig {
        minSdk = 28
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
        // The prebuilt .so is arm64 only; other ABIs are a matter of adding
        // targets to scripts/build-android.sh.
        ndk { abiFilters += "arm64-v8a" }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    // libmvt_render_jni.so lands in the default src/main/jniLibs, put there by
    // ../../scripts/build-android.sh.
}

dependencies {
    androidTestImplementation("androidx.test:runner:1.7.0")
    androidTestImplementation("androidx.test.ext:junit:1.3.0")
    androidTestImplementation("junit:junit:4.13.2")
}
