package dev.leptoskit.quant

import android.app.Activity
import android.content.Intent
import android.os.Build
import android.os.Bundle
import android.window.OnBackInvokedDispatcher
import dev.leptoskit.bridge.NativeActivityResult
import dev.leptoskit.bridge.NativeActivityResultBroker
import dev.leptoskit.bridge.NativeActivityResultHost
import dev.leptoskit.bridge.NativePermissionBroker
import dev.leptoskit.bridge.NativePermissionHost
import dev.leptoskit.mobile.GeneratedPlugins
import dev.leptoskit.mobile.LeptosKitApplicationView

class MainActivity :
    Activity(),
    NativePermissionHost,
    NativeActivityResultHost {
    private var content: LeptosKitApplicationView? = null
    private val requests = NativeRequests(this)

    override suspend fun requestNativePermissions(permissions: Array<String>): IntArray = requests.permissions.request(permissions)

    override suspend fun launchPluginActivity(intent: Intent): NativeActivityResult = requests.activityResults.launch(intent)

    @Deprecated("Android platform callback; delegated to the owned activity-result broker")
    override fun onActivityResult(
        requestCode: Int,
        resultCode: Int,
        data: Intent?,
    ) {
        super.onActivityResult(requestCode, resultCode, data)
        requests.activityResults.onResult(requestCode, resultCode, data)
    }

    override fun onRequestPermissionsResult(
        requestCode: Int,
        permissions: Array<out String>,
        grantResults: IntArray,
    ) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults)
        requests.permissions.onResult(requestCode, permissions, grantResults)
    }

    override fun onCreate(state: Bundle?) {
        super.onCreate(state)
        requests.restoreState(state)
        content = applicationView(this)
        setContentView(content)
        noteAppShortcutLaunch(intent)
        QuantLinkPlugin.noteIntentIfPresent(intent)

        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            // Android 13 and later deliver Back only here: the manifest opts in
            // with enableOnBackInvokedCallback, and Android 16 requires it.
            onBackInvokedDispatcher.registerOnBackInvokedCallback(OnBackInvokedDispatcher.PRIORITY_DEFAULT) {
                offerSystemBack()
            }
        }
    }

    @Deprecated("Android 12 and earlier deliver Back here; later versions use the callback registered in onCreate")
    override fun onBackPressed() = offerSystemBack()

    /**
     * Offers Back to the app's file router first, and leaves only when it has
     * nothing to go back to. The router answers from the guest thread, a moment
     * later. Only it knows whether it can go back, so the Back callback stays
     * registered and Android never previews going home; a Back that arrives
     * while the router is still answering the last one waits its turn.
     */
    private fun offerSystemBack() {
        val view = content ?: return leave()

        view.offerSystemBack { wentBack -> if (!wentBack) leave() }
    }

    /** Does what Back does when nothing handles it: finishes, or moves a root task to the back. */
    private fun leave() {
        if (isFinishing || isDestroyed) return

        @Suppress("DEPRECATION")
        super.onBackPressed()
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        noteAppShortcutLaunch(intent)
        QuantLinkPlugin.noteIntentIfPresent(intent)
    }

    override fun onSaveInstanceState(state: Bundle) {
        requests.saveState(state)
        super.onSaveInstanceState(state)
    }

    override fun onResume() {
        super.onResume()
        requests.setActive(true)
    }

    override fun onPause() {
        requests.setActive(false)
        super.onPause()
    }

    override fun onDestroy() {
        requests.close()
        content?.close()
        content = null
        super.onDestroy()
    }
}

private class NativeRequests(
    private val activity: Activity,
) {
    val permissions by lazy { NativePermissionBroker(activity) }
    val activityResults by lazy { NativeActivityResultBroker(activity) }

    fun restoreState(state: Bundle?) {
        permissions.restoreState(state)
        activityResults.restoreState(state)
    }

    fun saveState(state: Bundle) {
        permissions.saveState(state)
        activityResults.saveState(state)
    }

    fun setActive(active: Boolean) {
        permissions.setActive(active)
        activityResults.setActive(active)
    }

    fun close() {
        permissions.close()
        activityResults.close()
    }
}

private fun applicationView(activity: Activity): LeptosKitApplicationView {
    val embedded = activity.assets.open("app.lkb").use { it.readBytes() }
    val configuration = activity.assets.open("codepush.json").use { it.readBytes() }

    return LeptosKitApplicationView(activity, embedded, configuration) { bridge, dispatcher ->
        GeneratedPlugins.register(activity, bridge, dispatcher)
        // App-owned: the generated plugin set has no inbound link handler.
        dispatcher.register(bridge, QuantLinkPlugin())
    }
}

private fun noteAppShortcutLaunch(intent: Intent?) {
    // Soft-resolve so apps that omit `app-shortcuts` still compile.
    try {
        val plugin =
            Class.forName("dev.leptoskit.plugins.appshortcuts.AppShortcutsPlugin")
        plugin.getMethod("noteLaunchIfPresent", Intent::class.java).invoke(null, intent)
    } catch (_: ClassNotFoundException) {
    } catch (_: ReflectiveOperationException) {
    }
}
