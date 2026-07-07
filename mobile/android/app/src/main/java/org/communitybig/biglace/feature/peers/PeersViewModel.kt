package org.communitybig.biglace.feature.peers

import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.flow.SharingStarted
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.combine
import kotlinx.coroutines.flow.flatMapLatest
import kotlinx.coroutines.flow.stateIn
import kotlinx.coroutines.launch
import org.communitybig.biglace.AppContainer
import org.communitybig.biglace.core.mesh.MeshState
import org.communitybig.biglace.core.mesh.Peer

data class PeerItem(
    val peer: Peer,
    val isFavorite: Boolean,
    /** Per-peer SSH login override the user set, if any. */
    val override: String?,
)

data class PeersUi(
    val state: MeshState = MeshState.Disconnected,
    val items: List<PeerItem> = emptyList(),
)

@OptIn(ExperimentalCoroutinesApi::class)
class PeersViewModel(private val container: AppContainer) : ViewModel() {

    val ui: StateFlow<PeersUi> = combine(
        container.activeBackend.flatMapLatest { it.state },
        container.activeBackend.flatMapLatest { it.peers },
        container.settings.favorites,
        container.settings.peerOverrides,
    ) { state, peers, favorites, overrides ->
        // Favorites → online → alphabetical, same precedence as the desktop list.
        val items = peers
            .map { PeerItem(it, favorites.contains(it.hostname), overrides[it.hostname]) }
            .sortedWith(
                compareByDescending<PeerItem> { it.isFavorite }
                    .thenByDescending { it.peer.online }
                    .thenBy { it.peer.hostname },
            )
        PeersUi(state, items)
    }.stateIn(viewModelScope, SharingStarted.WhileSubscribed(5_000), PeersUi())

    fun refresh() = viewModelScope.launch {
        container.activeBackend.value.refresh()
    }

    /** Whether there's enough saved config to attempt a connect. */
    fun canConnect(): Boolean =
        container.settings.serverUrl.isNotBlank() && container.secrets.authKey.isNotBlank()

    // Connect/disconnect via the foreground service so the tunnel survives the
    // app being closed and shows a persistent notification.
    fun connect() = container.requestConnect()

    fun disconnect() = container.requestDisconnect()

    fun toggleFavorite(hostname: String) = container.settings.toggleFavorite(hostname)

    fun setOverride(hostname: String, login: String) = container.settings.setOverride(hostname, login)
}
