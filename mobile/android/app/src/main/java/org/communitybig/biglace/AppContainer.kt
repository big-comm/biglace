package org.communitybig.biglace

import android.content.Context
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import org.communitybig.biglace.core.data.SecretStore
import org.communitybig.biglace.core.data.SettingsStore
import org.communitybig.biglace.core.mesh.MeshBackend
import org.communitybig.biglace.core.mesh.TsnetMeshBackend
import org.communitybig.biglace.core.panel.PanelClient
import org.communitybig.biglace.service.MeshService

/** A peer the user asked to open a terminal / files for, handed across tabs. */
data class PendingTarget(val host: String, val user: String, val wantFiles: Boolean)

/**
 * Manual dependency container (no Hilt yet). The real backend is
 * [TsnetMeshBackend] — the phone joins the tailnet in-process via the embedded
 * tsnet engine, so the app shows the user's ACTUAL network and can reach
 * tailnet-only peers for SSH/SFTP.
 */
class AppContainer(context: Context) {
    val appContext: Context = context.applicationContext

    val settings = SettingsStore(context)
    val secrets = SecretStore(context)
    val panel = PanelClient()

    private val tsnet = TsnetMeshBackend(context.filesDir.resolve("tsnet").absolutePath)

    val activeBackend: StateFlow<MeshBackend> = MutableStateFlow<MeshBackend>(tsnet).asStateFlow()

    /** Local 127.0.0.1 port that tunnels to `host:port` over the tailnet. */
    suspend fun forward(host: String, port: Int): Int = tsnet.forward(host, port)

    /** Set by a peer row's terminal/files button; read by the target tab. */
    val pendingTarget = MutableStateFlow<PendingTarget?>(null)

    fun hasConnectConfig(): Boolean =
        settings.serverUrl.isNotBlank() && secrets.authKey.isNotBlank()

    /** Connect through the foreground service so the tunnel survives app close. */
    fun requestConnect() {
        if (hasConnectConfig()) MeshService.connect(appContext)
    }

    fun requestDisconnect() = MeshService.disconnect(appContext)
}
