package org.communitybig.biglace

import android.Manifest
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.contract.ActivityResultContracts
import androidx.core.content.ContextCompat
import androidx.lifecycle.lifecycleScope
import kotlinx.coroutines.launch
import org.communitybig.biglace.ui.BigLaceApp

class MainActivity : ComponentActivity() {
    private lateinit var container: AppContainer

    private val requestNotifications =
        registerForActivityResult(ActivityResultContracts.RequestPermission()) {
            if (::container.isInitialized && container.hasConnectConfig()) {
                container.connectNow()
            }
        }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        container = (application as BigLaceApplication).container

        // Ask for notification access only when a connection is requested.
        lifecycleScope.launch {
            while (true) {
                container.connectRequests.receive()
                if (Build.VERSION.SDK_INT >= 33 &&
                    ContextCompat.checkSelfPermission(
                        this@MainActivity,
                        Manifest.permission.POST_NOTIFICATIONS,
                    ) != PackageManager.PERMISSION_GRANTED
                ) {
                    requestNotifications.launch(Manifest.permission.POST_NOTIFICATIONS)
                } else {
                    container.connectNow()
                }
            }
        }

        // Auto-connect on app launch when the user enabled it.
        if (container.settings.autoConnect && container.hasConnectConfig()) {
            container.requestConnect()
        }

        setContent { BigLaceApp(container) }
    }
}
