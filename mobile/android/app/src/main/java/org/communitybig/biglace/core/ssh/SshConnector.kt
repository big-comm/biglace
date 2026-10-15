package org.communitybig.biglace.core.ssh

import kotlinx.coroutines.CancellationException
import net.schmizz.sshj.DefaultConfig
import net.schmizz.sshj.SSHClient
import org.communitybig.biglace.AppContainer
import org.communitybig.biglace.R
import java.io.IOException

/** The host presented a different key than the one trusted on first use. */
class HostKeyChangedException(val host: String, message: String) : Exception(message)

/**
 * Opens an authenticated SSH transport to a tailnet peer: tsnet port-forward,
 * TOFU host-key check, then auth according to [AuthMode]. Shared by the
 * terminal, the file manager and their "Test" buttons.
 */
internal object SshConnector {
    class Connection(val client: SSHClient, val authMethod: String)

    /**
     * Blocking; call from Dispatchers.IO. [onClient] receives the client before
     * the handshake so the caller can abort a connection attempt by closing it.
     * On failure the client is already closed.
     */
    suspend fun connect(
        container: AppContainer,
        host: String,
        port: Int,
        user: String,
        password: String,
        mode: AuthMode,
        onClient: (SSHClient) -> Unit = {},
    ): Connection {
        val local = try {
            container.forward(host, port)
        } catch (e: CancellationException) {
            throw e
        } catch (e: Exception) {
            // The engine isn't running: say what to do instead of surfacing
            // the bridge's internal "tsbridge: not started".
            if (e.message.orEmpty().contains("not started")) {
                throw IOException(container.appContext.getString(R.string.ssh_mesh_not_connected), e)
            }
            throw e
        }
        val client = SSHClient(DefaultConfig())
        onClient(client)
        val verifier = TofuHostKeyVerifier(host, container.secrets)
        client.addHostKeyVerifier(verifier)
        try {
            client.connect("127.0.0.1", local)
            val method = SshAuth.authenticate(
                client, user, password, container.secrets.sshPrivateKey, mode, container.appContext,
            )
            if (password.isNotBlank() && mode != AuthMode.KEY) container.secrets.setSshPassword(host, password)
            if (user.isNotBlank()) container.secrets.setSshUser(host, user)
            return Connection(client, method)
        } catch (e: Exception) {
            runCatching { client.disconnect() }
            verifier.rejected?.let { fingerprint ->
                throw HostKeyChangedException(
                    host,
                    container.appContext.getString(R.string.ssh_host_key_changed, host, fingerprint),
                )
            }
            throw e
        }
    }
}
