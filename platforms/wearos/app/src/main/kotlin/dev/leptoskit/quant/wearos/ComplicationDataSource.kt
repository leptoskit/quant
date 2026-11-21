package dev.leptoskit.quant.wearos

import androidx.wear.watchface.complications.data.ComplicationData
import androidx.wear.watchface.complications.data.ComplicationType
import androidx.wear.watchface.complications.data.PlainComplicationText
import androidx.wear.watchface.complications.data.ShortTextComplicationData
import androidx.wear.watchface.complications.datasource.ComplicationRequest
import androidx.wear.watchface.complications.datasource.SuspendingComplicationDataSourceService

/**
 * Watch-face complication reporting latest heart rate or recording state.
 */
class ComplicationDataSource : SuspendingComplicationDataSourceService() {
    override fun getPreviewData(type: ComplicationType): ComplicationData? =
        buildShortText("72", "Quant")

    override suspend fun onComplicationRequest(request: ComplicationRequest): ComplicationData? {
        if (request.complicationType != ComplicationType.SHORT_TEXT) {
            return null
        }
        val prefs = getSharedPreferences("quant_watch_status", MODE_PRIVATE)
        val badge = prefs.getString("badge", "REC") ?: "REC"
        return buildShortText(badge, "Quant")
    }

    private fun buildShortText(value: String, title: String): ComplicationData =
        ShortTextComplicationData.Builder(
            text = PlainComplicationText.Builder(value).build(),
            contentDescription = PlainComplicationText.Builder("$title $value").build(),
        )
            .setTitle(PlainComplicationText.Builder(title).build())
            .build()
}
