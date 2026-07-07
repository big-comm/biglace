import org.jetbrains.kotlin.gradle.dsl.JvmTarget

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("org.jetbrains.kotlin.plugin.compose")
}

android {
    namespace = "org.communitybig.biglace"
    compileSdk = 36
    buildToolsVersion = "36.1.0"

    defaultConfig {
        applicationId = "org.communitybig.biglace"
        minSdk = 26
        targetSdk = 36
        versionCode = 23
        versionName = "0.7.5"
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"),
                "proguard-rules.pro",
            )
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    buildFeatures {
        compose = true
    }
}

kotlin {
    compilerOptions {
        jvmTarget.set(JvmTarget.JVM_17)
    }
}

// Standard Compose setup via the BOM: one pinned line keeps the whole Compose
// graph mutually consistent, and Material/lifecycle/activity are on stable
// releases that pair with it.
dependencies {
    implementation("androidx.core:core-ktx:1.15.0")
    implementation("androidx.activity:activity-compose:1.9.3")

    implementation(platform("androidx.compose:compose-bom:2024.12.01"))
    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.ui:ui-graphics")
    implementation("androidx.compose.ui:ui-tooling-preview")
    implementation("androidx.compose.foundation:foundation")
    implementation("androidx.compose.material3:material3")
    implementation("androidx.compose.material:material-icons-core")

    implementation("androidx.lifecycle:lifecycle-runtime-compose:2.8.7")
    implementation("androidx.lifecycle:lifecycle-viewmodel-compose:2.8.7")

    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.9.0")

    // Embedded Tailscale (tsnet) engine, built from mobile/tsbridge via gomobile.
    // Self-contained AAR (arm64-v8a + armeabi-v7a native libs) — real devices
    // only; x86 emulators lack the .so.
    implementation(files("libs/tsbridge.aar"))

    // SSH + SFTP for the terminal and file manager.
    implementation("com.hierynomus:sshj:0.38.0")
    implementation("org.slf4j:slf4j-nop:2.0.13")
    // Full BouncyCastle: Android's built-in "BC" provider is stripped and lacks
    // X25519 etc.; the app swaps in this one at startup (BigLaceApplication).
    implementation("org.bouncycastle:bcprov-jdk18on:1.78.1")

    debugImplementation("androidx.compose.ui:ui-tooling")
}
