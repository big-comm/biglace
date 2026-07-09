package org.communitybig.biglace.feature.settings

import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.SharingStarted
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.flatMapLatest
import kotlinx.coroutines.flow.stateIn
import kotlinx.coroutines.launch
import java.text.Normalizer
import java.util.Locale
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
        container.settings.hostname = sanitizeDeviceName(hostname)
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
    private val _keyError = MutableStateFlow<String?>(null)
    val keyError: StateFlow<String?> = _keyError.asStateFlow()

    fun generateSshKey() = viewModelScope.launch {
        if (_generating.value) return@launch
        _generating.value = true
        _keyError.value = null
        val comment = "biglace-${container.settings.hostname.ifBlank { "mobile" }}"
        try {
            val kp = kotlinx.coroutines.withContext(Dispatchers.Default) { SshKeys.generateRsa(comment) }
            container.secrets.setSshKeys(kp.privatePem, kp.publicOpenSsh)
            _sshPub.value = kp.publicOpenSsh
        } catch (e: CancellationException) {
            throw e
        } catch (e: Exception) {
            _keyError.value = e.message ?: text(org.communitybig.biglace.R.string.ssh_key_generation_failed)
        } finally {
            _generating.value = false
        }
    }

    fun clearTrustedSshHosts() = container.secrets.clearSshHostFingerprints()

    /** Panel sign-in: fetch a fresh pre-auth key and fill the form (like desktop). */
    suspend fun panelSignIn(url: String, user: String, password: String, node: String): Result<Unit> =
        try {
            val panelUrl = normalizePanelUrl(url)
            val deviceName = sanitizeDeviceName(node)
            require(deviceName.isNotBlank()) { text(org.communitybig.biglace.R.string.settings_device_name_required) }
            val resp = container.panel.requestPreauth(
                PanelCredentials(
                    url = panelUrl,
                    username = user.trim(),
                    password = password,
                    node = deviceName,
                    hostname = deviceName,
                ),
            )
            require(resp.authKey.isNotBlank()) { text(org.communitybig.biglace.R.string.panel_missing_authkey) }
            container.settings.panelUrl = panelUrl
            container.settings.panelUsername = user.trim()
            container.settings.serverUrl = sanitizeServerFromPanel(resp.serverUrl, panelUrl)
            container.settings.hostname = deviceName
            container.secrets.authKey = resp.authKey
            Result.success(Unit)
        } catch (e: CancellationException) {
            throw e
        } catch (e: Exception) {
            Result.failure(e)
        }

    private fun normalizeServer(input: String): String {
        val s = input.trim().trimEnd('/')
        if (s.isEmpty()) return ""
        return if (s.contains("://")) s else "https://$s"
    }

    private fun normalizePanelUrl(input: String): String {
        val normalized = normalizeServer(input)
        val uri = runCatching { java.net.URI(normalized) }.getOrNull()
        val host = uri?.host.orEmpty().trim('[', ']')
        val loopback = host.equals("localhost", true) || host == "::1" ||
            host.split('.').mapNotNull { it.toIntOrNull() }.let {
                it.size == 4 && it[0] == 127 && it.all { octet -> octet in 0..255 }
            }
        require(
            uri != null && host.isNotBlank() && uri.userInfo == null && uri.query == null &&
                uri.fragment == null && (uri.scheme.equals("https", true) ||
                (uri.scheme.equals("http", true) && loopback)),
        ) { text(org.communitybig.biglace.R.string.panel_https_required) }
        return normalized
    }

    private fun sanitizeServerFromPanel(server: String, panelUrl: String): String {
        val normalized = normalizeServer(server)
        val host = runCatching { java.net.URI(normalized).host.orEmpty() }.getOrDefault("")
        val loopback = host.equals("localhost", ignoreCase = true) ||
            host == "127.0.0.1" || host == "0.0.0.0" || host == "::1"
        return if (normalized.isBlank() || loopback) panelUrl else normalized
    }

    private fun text(id: Int): String = container.appContext.getString(id)
}

internal fun sanitizeDeviceName(input: String): String {
    val ascii = Normalizer.normalize(input, Normalizer.Form.NFD)
        .replace(Regex("\\p{M}+"), "")
        .lowercase(Locale.ROOT)
    return ascii
        .replace(Regex("[^a-z0-9]+"), "-")
        .replace(Regex("-+"), "-")
        .trim('-')
        .take(63)
        .trimEnd('-')
}
