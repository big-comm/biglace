package org.communitybig.biglace.feature.settings

import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.SharingStarted
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.flatMapLatest
import kotlinx.coroutines.flow.stateIn
import kotlinx.coroutines.launch
import org.communitybig.biglace.AppContainer
import org.communitybig.biglace.core.data.SshKeys
import org.communitybig.biglace.core.mesh.MeshState
import org.communitybig.biglace.core.panel.PanelCredentials

@OptIn(ExperimentalCoroutinesApi::class)
class SettingsViewModel(private val container: AppContainer) : ViewModel() {

    val state: StateFlow<MeshState> = container.activeBackend
        .flatMapLatest { it.state }
        .stateIn(viewModelScope, SharingStarted.WhileSubscribed(5_000), MeshState.Disconnected)

    // Initial form values, read once for prefill.
    fun initialServer() = container.settings.serverUrl
    fun initialAuthKey() = container.secrets.authKey
    fun initialHostname() = container.settings.hostname
    fun initialAutoConnect() = container.settings.autoConnect
    fun initialPanelUrl() = container.settings.panelUrl
    fun initialPanelUsername() = container.settings.panelUsername

    fun save(server: String, authKey: String, hostname: String, autoConnect: Boolean) {
        container.settings.serverUrl = normalizeServer(server)
        container.secrets.authKey = authKey.trim()
        container.settings.hostname = hostname.trim()
        container.settings.autoConnect = autoConnect
    }

    fun connect(server: String, authKey: String, hostname: String) {
        save(server, authKey, hostname, container.settings.autoConnect)
        // Route through the foreground service (survives app close, notification).
        container.requestConnect()
    }

    fun disconnect() = container.requestDisconnect()

    var connectOnBoot: Boolean
        get() = container.settings.connectOnBoot
        set(v) { container.settings.connectOnBoot = v }

    // ── SSH key ──────────────────────────────────────────────────────────────
    private val _sshPub = MutableStateFlow(container.secrets.sshPublicKey)
    val sshPublicKey: StateFlow<String> = _sshPub.asStateFlow()
    private val _generating = MutableStateFlow(false)
    val generatingKey: StateFlow<Boolean> = _generating.asStateFlow()

    fun generateSshKey() = viewModelScope.launch {
        if (_generating.value) return@launch
        _generating.value = true
        val comment = "biglace-${container.settings.hostname.ifBlank { "mobile" }}"
        val kp = kotlinx.coroutines.withContext(Dispatchers.Default) { SshKeys.generateRsa(comment) }
        container.secrets.setSshKeys(kp.privatePem, kp.publicOpenSsh)
        _sshPub.value = kp.publicOpenSsh
        _generating.value = false
    }

    /** Panel sign-in: fetch a fresh pre-auth key and fill the form (like desktop). */
    suspend fun panelSignIn(url: String, user: String, password: String, node: String): Result<Unit> =
        runCatching {
            val resp = container.panel.requestPreauth(
                PanelCredentials(url = url, username = user, password = password, node = node, hostname = node),
            )
            container.settings.panelUrl = url
            container.settings.panelUsername = user
            container.settings.serverUrl = resp.serverUrl.ifEmpty { container.settings.serverUrl }
            container.settings.hostname = node
            container.secrets.authKey = resp.authKey
            container.secrets.setPanelPassword(password)
        }

    private fun normalizeServer(input: String): String {
        val s = input.trim().trimEnd('/')
        if (s.isEmpty()) return ""
        return if (s.contains("://")) s else "https://$s"
    }
}
