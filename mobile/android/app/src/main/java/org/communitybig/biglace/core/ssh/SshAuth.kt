package org.communitybig.biglace.core.ssh

import net.schmizz.sshj.SSHClient
import net.schmizz.sshj.userauth.method.AuthKeyboardInteractive
import net.schmizz.sshj.userauth.method.AuthPassword
import net.schmizz.sshj.userauth.method.AuthPublickey
import net.schmizz.sshj.userauth.method.PasswordResponseProvider
import net.schmizz.sshj.userauth.password.PasswordFinder
import net.schmizz.sshj.userauth.password.PasswordUtils

/** Carries a human-readable auth transcript so the UI can show what happened. */
class SshAuthException(message: String) : Exception(message)

/**
 * Authenticate an sshj client according to [AuthMode], trying each method ON ITS
 * OWN so we can report which one succeeded ("publickey" / "password") and, on
 * failure, exactly why. In [AuthMode.KEY] there is NO password fallback — the key
 * error surfaces directly, which is what makes "is my key working?" answerable.
 */
object SshAuth {
    fun authenticate(
        ssh: SSHClient,
        user: String,
        password: String,
        privateKeyPem: String,
        mode: AuthMode = AuthMode.AUTO,
    ): String {
        val log = StringBuilder()
        val tryKey = mode != AuthMode.PASSWORD
        val tryPassword = mode != AuthMode.KEY

        if (tryKey) {
            when {
                privateKeyPem.isBlank() -> {
                    if (mode == AuthMode.KEY) {
                        throw SshAuthException(
                            "No SSH key configured.\nGenerate one in Settings → SSH key, then add its " +
                                "public key to the server's ~/.ssh/authorized_keys.",
                        )
                    }
                    log.append("• no SSH key configured\n")
                }
                else -> try {
                    val keys = ssh.loadKeys(privateKeyPem, null as String?, null as PasswordFinder?)
                    ssh.auth(user, AuthPublickey(keys))
                    return "publickey"
                } catch (e: Exception) {
                    log.append("• publickey rejected: ${e.message}\n")
                    if (mode == AuthMode.KEY) throw failure(ssh, user, log, keyHint = true)
                }
            }
        }

        if (tryPassword) {
            when {
                password.isBlank() -> {
                    if (mode == AuthMode.PASSWORD) throw SshAuthException("No password entered.")
                    log.append("• no password entered\n")
                }
                else -> try {
                    ssh.auth(
                        user,
                        AuthPassword(PasswordUtils.createOneOff(password.toCharArray())),
                        AuthKeyboardInteractive(
                            PasswordResponseProvider(PasswordUtils.createOneOff(password.toCharArray())),
                        ),
                    )
                    return "password"
                } catch (e: Exception) {
                    log.append("• password rejected: ${e.message}\n")
                }
            }
        }

        throw failure(ssh, user, log, keyHint = mode == AuthMode.KEY)
    }

    private fun failure(ssh: SSHClient, user: String, log: StringBuilder, keyHint: Boolean): SshAuthException {
        val allowed = runCatching { ssh.userAuth?.allowedMethods?.joinToString(", ") }
            .getOrNull()?.ifBlank { "(none)" } ?: "?"
        return SshAuthException(
            buildString {
                append("Authentication failed for \"$user\".\n\n")
                append("Server accepts: [$allowed]\n\n")
                append(log)
                if (keyHint) {
                    append("\nFor key auth the server needs your public key on ONE line of ")
                    append("~/.ssh/authorized_keys — with a space before the comment — and ")
                    append("perms 700 on ~/.ssh and 600 on authorized_keys.")
                }
            },
        )
    }
}
