package org.communitybig.biglace.core.ssh

import android.content.Context
import net.schmizz.sshj.SSHClient
import net.schmizz.sshj.userauth.method.AuthKeyboardInteractive
import net.schmizz.sshj.userauth.method.AuthMethod
import net.schmizz.sshj.userauth.method.AuthPassword
import net.schmizz.sshj.userauth.method.AuthPublickey
import net.schmizz.sshj.userauth.method.PasswordResponseProvider
import net.schmizz.sshj.userauth.password.PasswordFinder
import net.schmizz.sshj.userauth.password.PasswordUtils
import org.communitybig.biglace.R

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
        mode: AuthMode,
        context: Context,
    ): String {
        val log = StringBuilder()
        if (mode == AuthMode.AUTO) {
            return authenticateAuto(ssh, user, password, privateKeyPem, log, context)
        }

        val tryKey = mode == AuthMode.KEY
        val tryPassword = mode == AuthMode.PASSWORD

        if (tryKey) {
            when {
                privateKeyPem.isBlank() -> {
                    if (mode == AuthMode.KEY) {
                        throw SshAuthException(
                            context.getString(R.string.ssh_no_key_configured),
                        )
                    }
                    log.append(context.getString(R.string.ssh_no_key_log)).append('\n')
                }
                else -> try {
                    val keys = ssh.loadKeys(privateKeyPem, null as String?, null as PasswordFinder?)
                    ssh.auth(user, AuthPublickey(keys))
                    return "publickey"
                } catch (e: Exception) {
                    log.append(context.getString(R.string.ssh_publickey_rejected, e.message)).append('\n')
                    if (mode == AuthMode.KEY) throw failure(ssh, user, log, keyHint = true, context)
                }
            }
        }

        if (tryPassword) {
            when {
                password.isBlank() -> {
                    if (mode == AuthMode.PASSWORD) {
                        throw SshAuthException(context.getString(R.string.ssh_no_password))
                    }
                    log.append(context.getString(R.string.ssh_no_password_log)).append('\n')
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
                    log.append(context.getString(R.string.ssh_password_rejected, e.message)).append('\n')
                }
            }
        }

        throw failure(ssh, user, log, keyHint = mode == AuthMode.KEY, context)
    }

    /** Negotiate all available methods in one SSH user-auth session. */
    private fun authenticateAuto(
        ssh: SSHClient,
        user: String,
        password: String,
        privateKeyPem: String,
        log: StringBuilder,
        context: Context,
    ): String {
        val methods = mutableListOf<AuthMethod>()
        var lastMethod = "auto"

        fun tracked(method: AuthMethod): AuthMethod = object : AuthMethod by method {
            override fun request() {
                lastMethod = method.name
                method.request()
            }
        }

        if (privateKeyPem.isBlank()) {
            log.append(context.getString(R.string.ssh_no_key_log)).append('\n')
        } else {
            try {
                val keys = ssh.loadKeys(privateKeyPem, null as String?, null as PasswordFinder?)
                methods += tracked(AuthPublickey(keys))
            } catch (e: Exception) {
                log.append(context.getString(R.string.ssh_publickey_rejected, e.message)).append('\n')
            }
        }

        if (password.isBlank()) {
            log.append(context.getString(R.string.ssh_no_password_log)).append('\n')
        } else {
            methods += tracked(AuthPassword(PasswordUtils.createOneOff(password.toCharArray())))
            methods += tracked(
                AuthKeyboardInteractive(
                    PasswordResponseProvider(PasswordUtils.createOneOff(password.toCharArray())),
                ),
            )
        }

        if (methods.isEmpty()) throw failure(ssh, user, log, keyHint = false, context)

        try {
            ssh.auth(user, methods)
            return lastMethod
        } catch (e: Exception) {
            log.append(context.getString(R.string.ssh_auto_rejected, e.message)).append('\n')
            throw failure(ssh, user, log, keyHint = false, context)
        }
    }

    private fun failure(
        ssh: SSHClient,
        user: String,
        log: StringBuilder,
        keyHint: Boolean,
        context: Context,
    ): SshAuthException {
        val allowed = runCatching { ssh.userAuth?.allowedMethods?.joinToString(", ") }
            .getOrNull()?.ifBlank { context.getString(R.string.ssh_none) } ?: "?"
        return SshAuthException(
            buildString {
                append(context.getString(R.string.ssh_auth_failed_for, user)).append("\n\n")
                append(context.getString(R.string.ssh_server_accepts, allowed)).append("\n\n")
                append(log)
                if (keyHint) {
                    append('\n').append(context.getString(R.string.ssh_key_auth_hint))
                }
            },
        )
    }
}
