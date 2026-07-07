package org.communitybig.biglace.core.mesh

/**
 * A device on the mesh, mirroring the desktop client's `Peer`
 * (src/tailscale.rs + mobile/ARCHITECTURE.md §4). Kept as a plain data class so
 * every [MeshBackend] — fake, companion, or embedded — produces the same shape.
 */
data class Peer(
    val hostname: String,
    val ipv4: String? = null,
    val ipv6: String? = null,
    val dnsName: String = "",
    val online: Boolean = false,
    val os: String = "",
    /** BigScale account that owns the device (LoginName via UserID). */
    val owner: String = "",
    /** SSH login: panel os-users map → tag:user-<x> → hostname (resolved by callers). */
    val sshUser: String? = null,
    val lastSeen: String? = null,
    val tags: List<String> = emptyList(),
    val exitNodeOffered: Boolean = false,
    val exitNodeActive: Boolean = false,
    /** Round-trip latency in ms, when measured (peer rows' subtitle). */
    val latencyMs: Int? = null,
) {
    /** First IP for display/SSH, IPv4 preferred. */
    val ip: String? get() = ipv4 ?: ipv6

    /**
     * Same precedence as desktop `Peer::display_name()`: first DNS label →
     * hostname → IP, so a panel rename surfaces immediately. Favorites/overrides
     * still key on [hostname], never on this.
     */
    val displayName: String
        get() = dnsName.trimEnd('.').substringBefore('.').ifEmpty { null }
            ?: hostname.ifEmpty { null }
            ?: ip
            ?: ""
}

/** Connection state of the whole mesh layer, surfaced to the UI. */
sealed interface MeshState {
    data object Disconnected : MeshState
    data object Connecting : MeshState
    data class Up(val selfName: String, val selfIp: String?) : MeshState
    data class Error(val message: String) : MeshState
}
