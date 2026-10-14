package org.communitybig.biglace.core.mesh

import kotlinx.coroutines.flow.StateFlow

/**
 * The single seam every mesh implementation sits behind (mobile/ARCHITECTURE.md
 * §1–2), so the UI and ViewModels never learn which one is active. The only
 * implementation today is [TsnetMeshBackend] (userspace tsnet, no device VPN).
 */
interface MeshBackend {
    /** Human-readable id for the settings picker / logs. */
    val id: String

    val state: StateFlow<MeshState>
    val peers: StateFlow<List<Peer>>

    /** Join the mesh. `authKey` may be null when re-using a cached registration. */
    suspend fun connect(server: String, authKey: String?, hostname: String)

    suspend fun disconnect()

    /**
     * Stop the engine after a failure while leaving the [MeshState.Error]
     * visible, so the user can read why the connection dropped and retry.
     */
    suspend fun halt()

    /** Re-read the peer list / status once. Safe to call from the UI. */
    suspend fun refresh()
}
