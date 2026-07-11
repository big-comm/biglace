// Top-level build file. Plugin versions are pinned here and applied in :app.
// Versions chosen to match a known-good, offline-cacheable toolchain
// (AGP 8.12.x, Kotlin 2.2.x, Compose 1.11.x / Material3 1.5.x).
plugins {
    id("com.android.application") version "8.12.3" apply false
    id("org.jetbrains.kotlin.android") version "2.2.20" apply false
    id("org.jetbrains.kotlin.plugin.compose") version "2.2.20" apply false
}
