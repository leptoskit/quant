import java.util.Properties

plugins { id("com.android.application") }

val leptoskitVersion =
    Properties().apply {
        file("../../../.leptoskit/android/version.properties").inputStream().use { load(it) }
    }

android {
    namespace = "dev.leptoskit.quant.wearos"
    ndkPath = file("../../../.leptoskit/android/ndk-path.txt").readText().trim()
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
        ndk { abiFilters += file("../../../.leptoskit/android/abi.txt").readText().trim() }
    }
    sourceSets["main"].apply {
        kotlin.directories += "../../../.leptoskit/android/sources"
        assets.directories += "../../../.leptoskit/android/mobile"
        res.directories += "../../../.leptoskit/android/res"
        manifest.srcFile("../../../.leptoskit/android/AndroidManifest.xml")
    }
    externalNativeBuild { cmake { path = file("../../../.leptoskit/android/CMakeLists.txt") } }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
}

dependencies {
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.10.2")
    implementation("androidx.wear:wear:1.3.0")
    implementation("androidx.wear.tiles:tiles:1.4.0")
    implementation("androidx.wear.watchface:watchface-complications-data-source-ktx:1.2.1")
    implementation("androidx.health:health-services-client:1.1.0-alpha05")
    file("../../../.leptoskit/android/plugin-dependencies.txt").readLines().forEach {
        implementation(it)
    }
}
