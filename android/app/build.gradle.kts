plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("org.jetbrains.kotlin.plugin.compose")
}

android {
    namespace = "se.frankling.time"
    compileSdk = 35

    // Pinned because AGP otherwise asks for a build-tools it happens to prefer
    // and tries to install it into the read-only Nix store SDK.
    buildToolsVersion = "35.0.0"

    defaultConfig {
        applicationId = "se.frankling.time"
        minSdk = 29
        targetSdk = 35
        // Android refuses to install a package whose versionCode is not greater
        // than the installed one, so every CI build has to get a fresh number.
        // The workflow run number is the only counter that is guaranteed to be
        // monotonic without committing a bumped file back to the repo.
        versionCode = System.getenv("ANDROID_VERSION_CODE")?.toIntOrNull() ?: 1
        versionName = "0.1.0"
    }

    // BuildConfig is off by default in AGP 8+, and versionName is shown in the
    // settings screen so the served APK can be compared against what's running.
    buildFeatures {
        compose = true
        buildConfig = true
    }

    signingConfigs {
        create("release") {
            // Supplied by CI from secrets; absent locally, in which case the
            // release build falls back to unsigned and `assembleDebug` is used.
            val store = System.getenv("KEYSTORE_PATH")
            if (store != null) {
                storeFile = file(store)
                storePassword = System.getenv("KEYSTORE_PASSWORD")
                keyAlias = System.getenv("KEY_ALIAS")
                keyPassword = System.getenv("KEY_PASSWORD")
            }
        }
    }

    buildTypes {
        release {
            // R8 full mode strips reflection-dependent code and produces
            // crashes that only appear in CI-built release APKs. Not worth it
            // for an app this small.
            isMinifyEnabled = false
            if (System.getenv("KEYSTORE_PATH") != null) {
                signingConfig = signingConfigs.getByName("release")
            }
        }
        debug {
            // So a debug build can sit alongside a release one without the
            // signature mismatch that forces an uninstall.
            applicationIdSuffix = ".debug"
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions { jvmTarget = "17" }

    testOptions { unitTests.isReturnDefaultValues = true }
}

dependencies {
    implementation(platform("androidx.compose:compose-bom:2024.10.01"))
    implementation("androidx.compose.material3:material3")
    implementation("androidx.compose.ui:ui-tooling-preview")
    implementation("androidx.activity:activity-compose:1.9.3")
    implementation("androidx.work:work-runtime-ktx:2.10.0")
    // FileProvider, for handing the downloaded APK to the package installer.
    implementation("androidx.core:core-ktx:1.13.1")
    debugImplementation("androidx.compose.ui:ui-tooling")
    testImplementation("junit:junit:4.13.2")
}
