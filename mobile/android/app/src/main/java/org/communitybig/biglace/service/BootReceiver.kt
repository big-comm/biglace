package org.communitybig.biglace.service

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import org.communitybig.biglace.BigLaceApplication

/** Starts the mesh tunnel on device boot when the user enabled "connect on boot". */
class BootReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        if (intent.action != Intent.ACTION_BOOT_COMPLETED) return
        val container = (context.applicationContext as BigLaceApplication).container
        val configured = container.settings.serverUrl.isNotBlank() && container.secrets.authKey.isNotBlank()
        if (container.settings.connectOnBoot && configured) {
            MeshService.connect(context)
        }
    }
}
