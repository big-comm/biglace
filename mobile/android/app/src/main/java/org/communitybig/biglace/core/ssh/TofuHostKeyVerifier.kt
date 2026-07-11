package org.communitybig.biglace.core.ssh

import net.schmizz.sshj.common.SecurityUtils
import net.schmizz.sshj.transport.verification.HostKeyVerifier
import org.communitybig.biglace.core.data.SecretStore
import java.security.PublicKey

/** Trust on first use, keyed by the tailnet peer identity rather than localhost. */
class TofuHostKeyVerifier(
    private val peerId: String,
    private val secrets: SecretStore,
) : HostKeyVerifier {
    override fun verify(hostname: String?, port: Int, key: PublicKey): Boolean {
        val fingerprint = SecurityUtils.getFingerprint(key)
        val known = secrets.sshHostFingerprint(peerId)
        if (known == null) {
            secrets.setSshHostFingerprint(peerId, fingerprint)
            return true
        }
        return known == fingerprint
    }

    override fun findExistingAlgorithms(hostname: String?, port: Int): List<String> = emptyList()
}
