package dev.leptoskit.quant.wearos

import android.os.Bundle
import androidx.activity.ComponentActivity
import dev.leptoskit.bridge.LeptosKitApplicationHost

/**
 * Wear OS entry point hosting Quant's Wasm `WatchScreen` and `quant-phone`
 * encrypted spool service on Pixel Watch, Samsung Galaxy Watch, and other
 * Wear OS 3+ devices.
 */
class MainActivity : ComponentActivity() {
    private var host: LeptosKitApplicationHost? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        host = LeptosKitApplicationHost.attach(this)
    }

    override fun onDestroy() {
        host?.close()
        host = null
        super.onDestroy()
    }
}
