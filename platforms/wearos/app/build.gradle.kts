import java.util.Properties

plugins { id("com.android.application") }

val leptoskitVersion =
    Properties().apply {
        file("../../../.leptoskit/wearos/version.properties").inputStream().use { load(it) }
    }

android {
    namespace = "dev.leptoskit.quant.wearos"
    ndkPath = file("../../../.leptoskit/wearos/ndk-path.txt").readText().trim()
    ndkVersion =
        Properties()
            .apply {
                file("$ndkPath/source.properties").inputStream().use { load(it) }
            }.getProperty("Pkg.Revision") ?: error("Selected Android NDK is missing Pkg.Revision")
    compileSdk = 36
    defaultConfig {
        applicationId = "dev.leptoskit.quant"
        // Wear OS 3+ (Pixel Watch, Samsung Galaxy Watch 4/5/6/7/Ultra, TicWatch Pro 5).
        minSdk = 30
        targetSdk = 36
        versionCode =
            (
                findProperty("leptoskit.versionCode") as String?
                    ?: leptoskitVersion.getProperty("versionCode")
            ).toInt()
        versionName = leptoskitVersion.getProperty("versionName")
        ndk { abiFilters += file("../../../.leptoskit/wearos/abi.txt").readText().trim() }
    }
    sourceSets["main"].apply {
        kotlin.directories += "../../../.leptoskit/wearos/sources"
        assets.directories += "../../../.leptoskit/wearos/mobile"
        res.directories += "../../../.leptoskit/wearos/res"
        manifest.srcFile("../../../.leptoskit/wearos/AndroidManifest.xml")
    }
    externalNativeBuild { cmake { path = file("../../../.leptoskit/wearos/CMakeLists.txt") } }

    signingConfigs {
        val keystore = providers.environmentVariable("LEPTOSKIT_ANDROID_KEYSTORE").orNull

        if (keystore != null) {
            create("release") {
                storeFile = file(keystore)
                storePassword =
                    providers.environmentVariable("LEPTOSKIT_ANDROID_KEYSTORE_PASSWORD").orNull
                keyAlias = providers.environmentVariable("LEPTOSKIT_ANDROID_KEY_ALIAS").orNull
                keyPassword =
                    providers.environmentVariable("LEPTOSKIT_ANDROID_KEY_PASSWORD").orNull
            }
        }
    }
    buildTypes {
        release {
            signingConfig = signingConfigs.findByName("release")
            isMinifyEnabled = true
            isShrinkResources = true
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"),
                "../../../.leptoskit/wearos/proguard-rules.pro",
                "proguard-rules.pro",
            )
        }
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
}

dependencies {
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.10.2")
    implementation("com.google.guava:guava:33.3.1-android")
    implementation("androidx.wear:wear:1.3.0")
    implementation("androidx.wear.protolayout:protolayout:1.2.1")
    implementation("androidx.wear.tiles:tiles:1.4.0")
    implementation("androidx.wear.watchface:watchface-complications-data-source-ktx:1.2.1")
    file("../../../.leptoskit/wearos/plugin-dependencies.txt").readLines().forEach {
        implementation(it)
    }
}
