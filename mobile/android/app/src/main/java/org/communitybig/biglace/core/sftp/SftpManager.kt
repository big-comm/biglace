package org.communitybig.biglace.core.sftp

import org.communitybig.biglace.core.mesh.Peer

/**
 * SFTP seam for the built-in file manager (mobile/ARCHITECTURE.md §7).
 *
 * SCAFFOLD — implemented in milestone M3 (mobile/ROADMAP.md) over the **same
 * sshj transport** the terminal uses, with a WorkManager-backed transfer queue
 * (progress notifications, resume, survive process death) and, as a stretch, a
 * DocumentsProvider exposing peers as roots in the system Files app.
 */
interface SftpManager {
    suspend fun list(peer: Peer, login: String, path: String): List<RemoteEntry>
    suspend fun download(peer: Peer, login: String, remotePath: String, into: java.io.File)
    suspend fun upload(peer: Peer, login: String, localFile: java.io.File, toDir: String)
}

data class RemoteEntry(
    val name: String,
    val isDirectory: Boolean,
    val sizeBytes: Long,
    val modifiedEpochSec: Long,
)
