package org.communitybig.biglace.core.mesh

import kotlinx.coroutines.flow.StateFlow

/**
 * The single seam every mesh implementation sits behind (mobile/ARCHITECTURE.md
 * §1–2). The three planned options —
 *   A) embedded libtailscale + VpnService (v1.0),
 *   B) companion to the official Tailscale app (MVP),
 *   C) userspace tsnet, no device VPN (fallback) —
 * all satisfy this interface, so the UI and ViewModels never learn which one is
 * active. Only the companion backend exists so far, and it cannot yet read the
 * live tailnet — that needs the embedded engine (M4).
 */
interface MeshBackend {
    /** Human-readable id for the settings picker / logs. */
    val id: String

    val state: StateFlow<MeshState>
    val peers: StateFlow<List<Peer>>

    /** Join the mesh. `authKey` may be null when re-using a cached registration. */
    suspend fun connect(server: String, authKey: String?, hostname: String)

    suspend fun disconnect()

    /** Re-read the peer list / status once. Safe to call from the UI. */
    suspend fun refresh()
}
