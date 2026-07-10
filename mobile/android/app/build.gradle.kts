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
        versionCode = 28
        versionName = "0.9.1"
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
    }

    buildTypes {
        release {
            isMinifyEnabled = true
            isShrinkResources = true
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"),
                "proguard-rules.pro",
            )
        }
    }

    splits {
        abi {
            isEnable = true
            reset()
            include("armeabi-v7a", "arm64-v8a", "x86", "x86_64")
            isUniversalApk = false
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

tasks.register<Exec>("buildTsbridge") {
    group = "build"
    description = "Rebuild the embedded tsnet AAR for every Android ABI."
    workingDir(rootProject.projectDir.resolve("../tsbridge"))
    commandLine("bash", "build-aar.sh")
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
    implementation("androidx.core:core-ktx:1.17.0")
    implementation("androidx.activity:activity-compose:1.13.0")

    implementation(platform("androidx.compose:compose-bom:2026.06.01"))
    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.ui:ui-graphics")
    implementation("androidx.compose.ui:ui-tooling-preview")
    implementation("androidx.compose.foundation:foundation")
    implementation("androidx.compose.material3:material3")
    implementation("androidx.compose.material:material-icons-core")

    implementation("androidx.lifecycle:lifecycle-runtime-compose:2.10.0")
    implementation("androidx.lifecycle:lifecycle-viewmodel-compose:2.10.0")

    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.11.0")

    // Embedded Tailscale (tsnet) engine, built from mobile/tsbridge via gomobile.
    // Self-contained AAR for physical devices and x86/x86_64 emulators.
    implementation(files("libs/tsbridge.aar"))

    // SSH + SFTP for the terminal and file manager.
    implementation("com.hierynomus:sshj:0.40.0")
    implementation("org.slf4j:slf4j-nop:2.0.18")
    // Full BouncyCastle: Android's built-in "BC" provider is stripped and lacks
    // X25519 etc.; the app swaps in this one at startup (BigLaceApplication).
    implementation("org.bouncycastle:bcprov-jdk18on:1.84")
    implementation("org.bouncycastle:bcpkix-jdk18on:1.84")

    debugImplementation("androidx.compose.ui:ui-tooling")
    testImplementation("junit:junit:4.13.2")
}
