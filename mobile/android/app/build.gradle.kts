import org.jetbrains.kotlin.gradle.dsl.JvmTarget
import java.util.Properties

// The product version users see — shared by the desktop and Android apps. It
// lives in APP_VERSION in src/main.rs, which gitrepo bumps on every commit
// (feat → minor, fix → patch), so there is a single number to keep in sync.
val appVersion: String = rootProject.file("../../src/main.rs")
    .takeIf { it.isFile }
    ?.readText()
    ?.let { Regex("""APP_VERSION: &str = "(\d+\.\d+\.\d+)"""").find(it)?.groupValues?.get(1) }
    ?: error("APP_VERSION not found in src/main.rs")

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
        // versionName is what users see (APP_VERSION, e.g. 1.11.1). versionCode
        // is the internal number Android compares to accept an update; CI
        // derives it from the release tag's date so it always grows
        // (.github/workflows/android.yml). 34 is the value for local builds.
        versionCode = (findProperty("biglace.versionCode") as String?)?.toInt() ?: 34
        versionName = (findProperty("biglace.versionName") as String?) ?: appVersion
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
    }

    // In-app updates only install over the existing app when every published APK
    // is signed with the SAME key. Put a dedicated key in keystore.properties
    // (gitignored; storeFile/storePassword/keyAlias/keyPassword). Without it the
    // release is signed with this machine's debug key — the one the 0.9.x test
    // builds already carry, so those installs can update in place.
    val keystoreProps = rootProject.file("keystore.properties")
    signingConfigs {
        if (keystoreProps.isFile) {
            create("release") {
                val p = Properties().apply { keystoreProps.inputStream().use { load(it) } }
                storeFile = rootProject.file(p.getProperty("storeFile"))
                storePassword = p.getProperty("storePassword")
                keyAlias = p.getProperty("keyAlias")
                keyPassword = p.getProperty("keyPassword")
            }
        }
    }

    buildTypes {
        release {
            signingConfig = signingConfigs.findByName("release") ?: signingConfigs.getByName("debug")
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
