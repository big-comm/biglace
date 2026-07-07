package org.communitybig.biglace.core.mesh

import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import org.communitybig.biglace.core.data.SettingsStore
import org.communitybig.biglace.core.panel.PanelClient

/**
 * MVP mesh backend (Option B, mobile/ARCHITECTURE.md §2): the **official
 * Tailscale app** owns the device VPN; BigLace Mobile only talks to the panel.
 * We don't drive VpnService here — connect just records intent and the panel
 * supplies device metadata.
 *
 * SKELETON: the panel's `os-users` endpoint returns a `hostname → os_user` map,
 * not a full peer list with online state. Turning that into rich [Peer]s needs
 * either a dedicated panel peer-list endpoint or the Headscale API — tracked as
 * open question #2 in mobile/ROADMAP.md. Until then this yields best-effort
 * peers (login known, online state unknown) so the HTTP path is exercised
 * end-to-end.
 */
class CompanionMeshBackend(
    private val panel: PanelClient,
    private val settings: SettingsStore,
) : MeshBackend {
    override val id = "companion"

    private val _state = MutableStateFlow<MeshState>(MeshState.Disconnected)
    override val state: StateFlow<MeshState> = _state.asStateFlow()

    private val _peers = MutableStateFlow<List<Peer>>(emptyList())
    override val peers: StateFlow<List<Peer>> = _peers.asStateFlow()

    override suspend fun connect(server: String, authKey: String?, hostname: String) {
        // In companion mode the official app performs the actual `up`. We record
        // the intended identity and treat the tunnel as up; a future version
        // deep-links to the Tailscale app and probes a tailnet address here.
        _state.value = MeshState.Up(selfName = hostname.ifEmpty { "this-phone" }, selfIp = null)
        refresh()
    }

    override suspend fun disconnect() {
        _state.value = MeshState.Disconnected
        _peers.value = emptyList()
    }

    override suspend fun refresh() {
        val base = settings.panelUrl.ifEmpty { settings.serverUrl }
        if (base.isEmpty()) return
        try {
            val map = panel.fetchOsUsers(base)
            _peers.value = map.map { (host, user) ->
                Peer(hostname = host, owner = user, sshUser = user, online = false)
            }.sortedBy { it.hostname }
        } catch (e: Exception) {
            _state.value = MeshState.Error(e.message ?: "panel unreachable")
        }
    }
}
