package org.communitybig.biglace.core.ssh

import org.communitybig.biglace.core.mesh.Peer

/**
 * SSH session seam for the built-in terminal (mobile/ARCHITECTURE.md §6).
 *
 * SCAFFOLD — the implementation lands in milestone M2 (mobile/ROADMAP.md) on top
 * of **sshj** (Apache-2.0): one transport per peer, multiplexed shell + SFTP
 * channels, keepalive, ed25519 keygen, and TOFU host-key verification. The
 * terminal emulator widget itself (Termux GPLv3 vs ConnectBot Apache-2.0) is the
 * open license decision to make at M2 kickoff — see mobile/TECH-STACK.md.
 *
 * Kept as an interface now so the terminal UI can be built against it and a fake
 * can drive previews.
 */
interface SshManager {
    /** Open (or reuse) a shell session to [peer], returning a handle to drive it. */
    suspend fun openShell(peer: Peer, login: String): SshSession

    /** Resolve the SSH login: user override → panel os_user → owner → hostname. */
    fun resolveLogin(peer: Peer, override: String?): String =
        override?.takeIf { it.isNotBlank() }
            ?: peer.sshUser?.takeIf { it.isNotBlank() }
            ?: peer.owner.takeIf { it.isNotBlank() }
            ?: peer.hostname
}

/** Live shell channel — writes keystrokes, streams output, propagates resize. */
interface SshSession {
    suspend fun write(bytes: ByteArray)
    fun onOutput(listener: (ByteArray) -> Unit)
    suspend fun resize(cols: Int, rows: Int)
    fun close()
}
