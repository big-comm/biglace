package org.communitybig.biglace.core.data

import android.util.Base64
import java.io.ByteArrayOutputStream
import java.math.BigInteger
import java.security.KeyPairGenerator
import java.security.PrivateKey
import java.security.interfaces.RSAPublicKey

/** A freshly generated SSH key pair. */
data class SshKeyPair(
    /** PKCS#8 PEM — what sshj loads for public-key auth. */
    val privatePem: String,
    /** OpenSSH one-liner — paste into a server's ~/.ssh/authorized_keys. */
    val publicOpenSsh: String,
)

/**
 * RSA SSH key generation + OpenSSH public-key encoding, using only the JDK
 * (no BouncyCastle needed here). The private key is PKCS#8 PEM (sshj reads it);
 * the public key is the standard `ssh-rsa AAAA... comment` line.
 */
object SshKeys {

    fun generateRsa(comment: String, bits: Int = 3072): SshKeyPair {
        val gen = KeyPairGenerator.getInstance("RSA")
        gen.initialize(bits)
        val kp = gen.generateKeyPair()
        return SshKeyPair(
            privatePem = pkcs8Pem(kp.private),
            publicOpenSsh = openSshRsa(kp.public as RSAPublicKey, comment),
        )
    }

    private fun pkcs8Pem(key: PrivateKey): String {
        val b64 = Base64.encodeToString(key.encoded, Base64.NO_WRAP)
        val body = b64.chunked(64).joinToString("\n")
        return "-----BEGIN PRIVATE KEY-----\n$body\n-----END PRIVATE KEY-----\n"
    }

    private fun openSshRsa(key: RSAPublicKey, comment: String): String {
        val blob = ByteArrayOutputStream().apply {
            writeSshString(this, "ssh-rsa".toByteArray(Charsets.US_ASCII))
            writeMpint(this, key.publicExponent)
            writeMpint(this, key.modulus)
        }.toByteArray()
        val b64 = Base64.encodeToString(blob, Base64.NO_WRAP)
        return "ssh-rsa $b64 ${sanitizeComment(comment)}"
    }

    /**
     * A key comment must be a single whitespace-free token: the device name can
     * contain spaces/uppercase ("Tales Cel"), which would leave a trailing/space
     * in the authorized_keys line and trip up some tooling. Collapse whitespace,
     * keep only a safe charset, and fall back to a sane default.
     */
    private fun sanitizeComment(comment: String): String =
        comment.trim()
            .replace(Regex("\\s+"), "-")
            .replace(Regex("[^A-Za-z0-9._@-]"), "")
            .trim('-')
            .ifBlank { "biglace-mobile" }

    private fun writeSshString(out: ByteArrayOutputStream, bytes: ByteArray) {
        writeUint32(out, bytes.size)
        out.write(bytes)
    }

    // SSH mpint: BigInteger.toByteArray() is already big-endian two's complement
    // with a leading 0x00 when the high bit is set — exactly the mpint format.
    private fun writeMpint(out: ByteArrayOutputStream, n: BigInteger) =
        writeSshString(out, n.toByteArray())

    private fun writeUint32(out: ByteArrayOutputStream, n: Int) {
        out.write((n ushr 24) and 0xff)
        out.write((n ushr 16) and 0xff)
        out.write((n ushr 8) and 0xff)
        out.write(n and 0xff)
    }
}
