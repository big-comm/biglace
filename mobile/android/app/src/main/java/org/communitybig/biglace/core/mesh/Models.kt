package org.communitybig.biglace.core.mesh

/**
 * A device on the mesh, mirroring the desktop client's `Peer`
 * (src/tailscale.rs + mobile/ARCHITECTURE.md §4). Kept as a plain data class so
 * every [MeshBackend] produces the same shape.
 */
data class Peer(
    /** Stable node identity (the node key). Hostnames are NOT unique on a tailnet. */
    val id: String = "",
    val hostname: String,
    val ipv4: String? = null,
    val ipv6: String? = null,
    val dnsName: String = "",
    val online: Boolean = false,
    val os: String = "",
    /** BigScale account that owns the device (LoginName via UserID). */
    val owner: String = "",
    /** SSH login from `tag:user-<x>`, when the node has one (callers fall back to owner/hostname). */
    val sshUser: String? = null,
    val lastSeen: String? = null,
    val tags: List<String> = emptyList(),
    val exitNodeOffered: Boolean = false,
    val exitNodeActive: Boolean = false,
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

    /** [message] is written for the user; [details] is the raw engine output. */
    data class Error(val message: String, val details: String = "") : MeshState
}

/** Connection failures the app can explain in plain words. */
enum class MeshErrorKind { AUTHKEY_NOT_FOUND, AUTHKEY_EXPIRED, AUTHKEY_USED, TIMEOUT, SERVER_UNREACHABLE }

/** Recognise a known failure in the engine's error text (control-server wording). */
internal fun classifyMeshError(raw: String): MeshErrorKind? {
    val text = raw.lowercase()
    val authKey = "auth-key" in text || "authkey" in text || "auth key" in text
    return when {
        authKey && "not found" in text -> MeshErrorKind.AUTHKEY_NOT_FOUND
        authKey && "expired" in text -> MeshErrorKind.AUTHKEY_EXPIRED
        authKey && "used" in text -> MeshErrorKind.AUTHKEY_USED
        "no such host" in text || "connection refused" in text || "network is unreachable" in text ||
            "certificate" in text -> MeshErrorKind.SERVER_UNREACHABLE
        "deadline exceeded" in text || "timeout" in text || "timed out" in text -> MeshErrorKind.TIMEOUT
        else -> null
    }
}
