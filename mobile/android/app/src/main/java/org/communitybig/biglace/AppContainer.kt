package org.communitybig.biglace

import android.content.Context
import android.net.ConnectivityManager
import android.net.Network
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.launch
import org.communitybig.biglace.core.data.SecretStore
import org.communitybig.biglace.core.data.SettingsStore
import org.communitybig.biglace.core.mesh.MeshBackend
import org.communitybig.biglace.core.mesh.TsnetMeshBackend
import org.communitybig.biglace.core.panel.PanelClient
import org.communitybig.biglace.core.update.AppUpdater
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
    val panel = PanelClient(context)

    private val tsnet = TsnetMeshBackend(context, context.filesDir.resolve("tsnet").absolutePath)

    /** Process-lifetime IO scope: work that must outlive a screen (closing sockets). */
    val ioScope = CoroutineScope(SupervisorJob() + Dispatchers.IO)

    /** In-app updates from GitHub Releases. */
    val updater = AppUpdater(context, settings, ioScope)

    /** Whether the UI is on screen; background status polling slows down without it. */
    val uiVisible = MutableStateFlow(false)

    val activeBackend: StateFlow<MeshBackend> = MutableStateFlow<MeshBackend>(tsnet).asStateFlow()

    /** Local 127.0.0.1 port that tunnels to `host:port` over the tailnet. */
    suspend fun forward(host: String, port: Int): Int = tsnet.forward(host, port)

    /** Set by a peer row's terminal/files button; read by the target tab. */
    val pendingTarget = MutableStateFlow<PendingTarget?>(null)
    internal val connectRequests = Channel<Unit>(Channel.CONFLATED)

    init {
        val connectivity = appContext.getSystemService(ConnectivityManager::class.java)
        connectivity.registerDefaultNetworkCallback(
            object : ConnectivityManager.NetworkCallback() {
                override fun onAvailable(network: Network) = updateNetworkInterfaces()
                override fun onLost(network: Network) = updateNetworkInterfaces()
                override fun onCapabilitiesChanged(
                    network: Network,
                    networkCapabilities: android.net.NetworkCapabilities,
                ) = updateNetworkInterfaces()
            },
        )
    }

    private fun updateNetworkInterfaces() {
        ioScope.launch { tsnet.onNetworkChanged() }
    }

    /** This device already joined the mesh and reconnects without a pre-auth key. */
    fun hasDeviceIdentity(): Boolean = tsnet.hasPersistedState()

    fun hasConnectConfig(): Boolean =
        settings.serverUrl.isNotBlank() && (secrets.authKey.isNotBlank() || tsnet.hasPersistedState())

    /** Connect through the foreground service so the tunnel survives app close. */
    fun requestConnect() {
        if (hasConnectConfig()) connectRequests.trySend(Unit)
    }

    internal fun connectNow() = MeshService.connect(appContext)

    fun requestDisconnect() = MeshService.disconnect(appContext)

    /**
     * Leave the mesh for good: stop the tunnel and delete this device's node
     * identity and enrollment key. SSH keys and saved logins are untouched.
     */
    suspend fun forgetDevice() {
        // Disconnected state makes the foreground service stop itself.
        tsnet.disconnect()
        tsnet.wipeState()
        secrets.authKey = ""
    }
}
