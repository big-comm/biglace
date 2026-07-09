package org.communitybig.biglace.service

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import androidx.core.app.NotificationCompat
import androidx.core.app.ServiceCompat
import androidx.core.content.ContextCompat
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.combine
import kotlinx.coroutines.launch
import org.communitybig.biglace.BigLaceApplication
import org.communitybig.biglace.MainActivity
import org.communitybig.biglace.R
import org.communitybig.biglace.core.mesh.MeshState

/**
 * Foreground service that owns the embedded tsnet tunnel. Running as a
 * foreground service (with a persistent notification) is what keeps the mesh
 * connected after the Activity is closed, and puts the status-bar icon up top.
 */
class MeshService : Service() {

    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main)
    private lateinit var app: BigLaceApplication
    private var observeJob: Job? = null
    private var connectionJob: Job? = null
    private var refreshJob: Job? = null

    override fun onCreate() {
        super.onCreate()
        app = application as BigLaceApplication
        createChannel()
    }

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        when (intent?.action) {
            ACTION_DISCONNECT -> {
                connectionJob?.cancel()
                connectionJob = scope.launch {
                    try {
                        app.container.activeBackend.value.disconnect()
                    } finally {
                        stopService()
                    }
                }
                return START_NOT_STICKY
            }
            ACTION_CONNECT -> {
                // Go foreground immediately (required within a few seconds of
                // startForegroundService), then connect + track state.
                goForeground(getString(R.string.svc_connecting))
                observeState()
                if (connectionJob?.isActive == true) return START_REDELIVER_INTENT
                val c = app.container
                connectionJob = scope.launch {
                    c.activeBackend.value.connect(
                        c.settings.serverUrl,
                        c.secrets.authKey,
                        c.settings.hostname,
                    )
                    if (c.activeBackend.value.state.value is MeshState.Up) {
                        // The node key in tsnet's state store is enough for future
                        // reconnects; do not retain a reusable enrollment secret.
                        c.secrets.authKey = ""
                        startRefreshLoop()
                    } else if (c.activeBackend.value.state.value is MeshState.Error) {
                        try {
                            c.activeBackend.value.disconnect()
                        } finally {
                            stopService()
                        }
                    }
                }
            }
            else -> {
                stopSelf(startId)
                return START_NOT_STICKY
            }
        }
        return START_REDELIVER_INTENT
    }

    private fun observeState() {
        observeJob?.cancel()
        val backend = app.container.activeBackend.value
        // The backend starts Disconnected; don't stop the service on that initial
        // value — only once it has actually been active and then drops.
        var wasActive = false
        observeJob = scope.launch {
            combine(backend.state, backend.peers) { s, p -> s to p }.collect { (state, peers) ->
                when (state) {
                    is MeshState.Up -> {
                        wasActive = true
                        val online = peers.count { it.online }
                        goForeground(resources.getQuantityString(R.plurals.svc_connected, online, online))
                    }
                    MeshState.Connecting -> {
                        wasActive = true
                        goForeground(getString(R.string.svc_connecting))
                    }
                    is MeshState.Error -> {
                        wasActive = true
                        goForeground(getString(R.string.svc_error, state.message.take(80)))
                        if (connectionJob?.isCompleted == true) {
                            try {
                                backend.disconnect()
                            } finally {
                                stopService()
                            }
                        }
                    }
                    MeshState.Disconnected -> if (wasActive) stopService()
                }
            }
        }
    }

    private fun startRefreshLoop() {
        refreshJob?.cancel()
        refreshJob = scope.launch {
            while (true) {
                delay(15_000)
                val backend = app.container.activeBackend.value
                if (backend.state.value !is MeshState.Up) return@launch
                backend.refresh()
            }
        }
    }

    private fun goForeground(text: String) {
        val type = if (Build.VERSION.SDK_INT >= 34) ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE else 0
        ServiceCompat.startForeground(this, NOTIF_ID, buildNotification(text), type)
    }

    private fun buildNotification(text: String): Notification {
        val open = PendingIntent.getActivity(
            this, 0, Intent(this, MainActivity::class.java),
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
        val disconnect = PendingIntent.getService(
            this, 1, Intent(this, MeshService::class.java).setAction(ACTION_DISCONNECT),
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
        return NotificationCompat.Builder(this, CHANNEL_ID)
            .setSmallIcon(R.drawable.ic_stat_mesh)
            .setColor(ContextCompat.getColor(this, R.color.biglace_brand_blue))
            .setContentTitle(getString(R.string.app_name))
            .setContentText(text)
            .setContentIntent(open)
            .setOngoing(true)
            .setOnlyAlertOnce(true)
            .setCategory(NotificationCompat.CATEGORY_SERVICE)
            .addAction(0, getString(R.string.settings_disconnect), disconnect)
            .build()
    }

    private fun stopService() {
        observeJob?.cancel()
        refreshJob?.cancel()
        ServiceCompat.stopForeground(this, ServiceCompat.STOP_FOREGROUND_REMOVE)
        stopSelf()
    }

    private fun createChannel() {
        val nm = getSystemService(NotificationManager::class.java)
        val ch = NotificationChannel(CHANNEL_ID, getString(R.string.svc_channel_name), NotificationManager.IMPORTANCE_LOW)
        ch.description = getString(R.string.svc_channel_desc)
        nm.createNotificationChannel(ch)
    }

    override fun onDestroy() {
        observeJob?.cancel()
        refreshJob?.cancel()
        connectionJob?.cancel()
        scope.cancel()
        super.onDestroy()
    }

    companion object {
        const val ACTION_CONNECT = "org.communitybig.biglace.CONNECT"
        const val ACTION_DISCONNECT = "org.communitybig.biglace.DISCONNECT"
        private const val CHANNEL_ID = "mesh"
        private const val NOTIF_ID = 1

        /** Start the tunnel via a foreground service (survives app close). */
        fun connect(context: Context) {
            val i = Intent(context, MeshService::class.java).setAction(ACTION_CONNECT)
            ContextCompat.startForegroundService(context, i)
        }

        fun disconnect(context: Context) {
            val i = Intent(context, MeshService::class.java).setAction(ACTION_DISCONNECT)
            context.startService(i)
        }
    }
}
