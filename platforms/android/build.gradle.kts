plugins {
    id("com.android.application") version "9.0.1" apply false
    // Applied by the app module only when leptoskit's generated
    // configuration lists it, which happens when [native.android] push
    // opts the app into push enrollment. Quant does not.
    id("com.google.gms.google-services") version "4.6.0" apply false
}
