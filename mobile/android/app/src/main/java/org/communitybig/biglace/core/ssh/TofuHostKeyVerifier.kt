package org.communitybig.biglace.core.ssh

import android.util.Base64
import net.schmizz.sshj.common.Buffer
import net.schmizz.sshj.common.SecurityUtils
import net.schmizz.sshj.transport.verification.HostKeyVerifier
import org.communitybig.biglace.core.data.SecretStore
import java.security.MessageDigest
import java.security.PublicKey

/** Trust on first use, keyed by the tailnet peer identity rather than localhost. */
class TofuHostKeyVerifier(
    private val peerId: String,
    private val secrets: SecretStore,
) : HostKeyVerifier {
    /** Fingerprint the host presented when it did not match the trusted one. */
    @Volatile var rejected: String? = null
        private set

    override fun verify(hostname: String?, port: Int, key: PublicKey): Boolean {
        val fingerprint = sha256Fingerprint(key)
        val known = secrets.sshHostFingerprint(peerId)
        if (known == null || known == fingerprint) {
            if (known == null) secrets.setSshHostFingerprint(peerId, fingerprint)
            return true
        }
        // Hosts trusted by older versions were stored as MD5; upgrade in place.
        if (!known.startsWith(SHA256_PREFIX) && known == SecurityUtils.getFingerprint(key)) {
            secrets.setSshHostFingerprint(peerId, fingerprint)
            return true
        }
        rejected = fingerprint
        return false
    }

    override fun findExistingAlgorithms(hostname: String?, port: Int): List<String> = emptyList()

    /** Same `SHA256:<base64>` form `ssh-keygen -l` prints, so users can compare. */
    private fun sha256Fingerprint(key: PublicKey): String {
        val blob = Buffer.PlainBuffer().putPublicKey(key).compactData
        val digest = MessageDigest.getInstance("SHA-256").digest(blob)
        return SHA256_PREFIX + Base64.encodeToString(digest, Base64.NO_WRAP or Base64.NO_PADDING)
    }

    private companion object {
        const val SHA256_PREFIX = "SHA256:"
    }
}
