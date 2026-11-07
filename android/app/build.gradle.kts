import java.util.Base64

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("org.jetbrains.kotlin.plugin.compose")
}

android {
    namespace = "dev.phonecast.sender"
    compileSdk = 35

    defaultConfig {
        applicationId = "dev.phonecast.sender"
        // AudioPlaybackCaptureConfiguration requires API 29 (Android 10).
        minSdk = 29
        targetSdk = 35
        versionCode = 1
        versionName = "0.1.0"
    }

    signingConfigs {
        // CI provides the keystore as a base64 env var (GitHub Actions
        // secret). When absent (local dev), release builds fall back to the
        // debug key below so `assembleRelease` still works.
        val ksB64 = System.getenv("PHONECAST_KEYSTORE_B64")
        if (!ksB64.isNullOrBlank()) {
            create("phonecast") {
                val ksFile = layout.buildDirectory.file("phonecast-release.keystore")
                    .get().asFile
                ksFile.writeBytes(Base64.getDecoder().decode(ksB64))
                storeFile = ksFile
                storePassword = System.getenv("PHONECAST_KEYSTORE_PASSWORD")
                keyAlias = "phonecast"
                keyPassword = System.getenv("PHONECAST_KEYSTORE_PASSWORD")
            }
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            signingConfig = signingConfigs.findByName("phonecast")
                ?: signingConfigs.getByName("debug")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    kotlin {
        compilerOptions {
            jvmTarget.set(org.jetbrains.kotlin.gradle.dsl.JvmTarget.JVM_17)
        }
    }

    buildFeatures {
        compose = true
    }
}

dependencies {
    // Compose BOM keeps all compose artifacts on matching versions.
    val bom = platform("androidx.compose:compose-bom:2024.10.01")
    implementation(bom)
    androidTestImplementation(bom)

    implementation("androidx.core:core-ktx:1.15.0")
    implementation("androidx.activity:activity-compose:1.9.3")
    implementation("androidx.lifecycle:lifecycle-runtime-compose:2.8.7")
    implementation("androidx.compose.material3:material3")
    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.ui:ui-tooling-preview")
    debugImplementation("androidx.compose.ui:ui-tooling")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.9.0")

    testImplementation("junit:junit:4.13.2")
}
