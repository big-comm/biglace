package org.communitybig.biglace.feature.terminal

import androidx.compose.ui.text.AnnotatedString
import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import net.schmizz.sshj.DefaultConfig
import net.schmizz.sshj.SSHClient
import net.schmizz.sshj.connection.channel.direct.Session
import net.schmizz.sshj.transport.verification.PromiscuousVerifier
import org.communitybig.biglace.AppContainer
import org.communitybig.biglace.core.ssh.AuthMode
import org.communitybig.biglace.core.ssh.ProbeResult
import org.communitybig.biglace.core.ssh.SshAuth
import java.io.OutputStream

sealed interface TermStatus {
    data object Idle : TermStatus
    data object Connecting : TermStatus
    data object Connected : TermStatus
    data class Error(val message: String) : TermStatus
}

/**
 * A real interactive SSH shell over the tailnet. [AppContainer.forward] opens a
 * local port tunnelling to the peer through the embedded tsnet engine; sshj
 * drives a PTY-backed shell on it, and bytes flow through [TerminalEmulator]
 * (a grid VT emulator with colours). Auth follows the chosen [AuthMode], and a
 * separate [probe] tests credentials without opening a shell.
 */
class TerminalViewModel(private val container: AppContainer) : ViewModel() {

    private val emulator = TerminalEmulator()

    private val _screen = MutableStateFlow(AnnotatedString(""))
    val screen: StateFlow<AnnotatedString> = _screen.asStateFlow()

    private val _status = MutableStateFlow<TermStatus>(TermStatus.Idle)
    val status: StateFlow<TermStatus> = _status.asStateFlow()

    private val _testing = MutableStateFlow(false)
    val testing: StateFlow<Boolean> = _testing.asStateFlow()

    private val _probe = MutableStateFlow<ProbeResult?>(null)
    val probe: StateFlow<ProbeResult?> = _probe.asStateFlow()

    private var ssh: SSHClient? = null
    private var session: Session? = null
    private var outStream: OutputStream? = null
    private var renderJob: Job? = null

    @Volatile private var dirty = false

    fun connect(host: String, port: Int, user: String, password: String, mode: AuthMode) {
        if (_status.value == TermStatus.Connecting || _status.value == TermStatus.Connected) return
        _status.value = TermStatus.Connecting
        emulator.clear()
        _screen.value = AnnotatedString("")
        startRenderPump()
        viewModelScope.launch(Dispatchers.IO) {
            try {
                val local = container.forward(host, port)
                val c = SSHClient(DefaultConfig())
                c.addHostKeyVerifier(PromiscuousVerifier())
                c.connect("127.0.0.1", local)
                val method = SshAuth.authenticate(c, user, password, container.secrets.sshPrivateKey, mode)
                if (password.isNotBlank() && mode != AuthMode.KEY) container.secrets.setSshPassword(host, password)
                if (user.isNotBlank()) container.secrets.setSshUser(host, user)
                val s = c.startSession()
                s.allocatePTY("xterm-256color", COLS, ROWS, 0, 0, emptyMap())
                val shell = s.startShell()
                ssh = c; session = s; outStream = shell.outputStream
                emulator.feed("\u001B[90m[BigLace] authenticated via $method\u001B[0m\r\n")
                dirty = true
                _status.value = TermStatus.Connected

                val buf = ByteArray(8192)
                val ins = shell.inputStream
                while (true) {
                    val n = ins.read(buf)
                    if (n < 0) break
                    emulator.feed(String(buf, 0, n, Charsets.UTF_8))
                    dirty = true
                }
                _status.value = TermStatus.Idle
            } catch (e: Exception) {
                _status.value = TermStatus.Error(e.message ?: "SSH connection failed")
            } finally {
                closeQuietly()
            }
        }
    }

    /** Test credentials only (open transport, authenticate, close) and report. */
    fun probe(host: String, port: Int, user: String, password: String, mode: AuthMode) {
        if (_testing.value) return
        _testing.value = true
        viewModelScope.launch(Dispatchers.IO) {
            var c: SSHClient? = null
            try {
                val local = container.forward(host, port)
                c = SSHClient(DefaultConfig())
                c.addHostKeyVerifier(PromiscuousVerifier())
                c.connect("127.0.0.1", local)
                val method = SshAuth.authenticate(c, user, password, container.secrets.sshPrivateKey, mode)
                if (password.isNotBlank() && mode != AuthMode.KEY) container.secrets.setSshPassword(host, password)
                if (user.isNotBlank()) container.secrets.setSshUser(host, user)
                _probe.value = ProbeResult(true, "Connected to $user@$host.\nAuthenticated via $method.")
            } catch (e: Throwable) {
                _probe.value = ProbeResult(false, e.message ?: "Connection failed.")
            } finally {
                runCatching { c?.disconnect() }
                _testing.value = false
            }
        }
    }

    fun clearProbe() { _probe.value = null }

    fun dismissError() { if (_status.value is TermStatus.Error) _status.value = TermStatus.Idle }

    /** 30 fps render pump: rebuild the coloured screen only when new bytes arrived. */
    private fun startRenderPump() {
        renderJob?.cancel()
        renderJob = viewModelScope.launch {
            while (isActive) {
                if (dirty) {
                    dirty = false
                    _screen.value = emulator.render()
                }
                delay(33)
            }
        }
    }

    /** Send raw text straight to the shell (used for typed characters). */
    fun sendRaw(text: String) {
        val os = outStream ?: return
        viewModelScope.launch(Dispatchers.IO) {
            runCatching { os.write(text.toByteArray(Charsets.UTF_8)); os.flush() }
        }
    }

    fun sendBytes(vararg bytes: Int) {
        val os = outStream ?: return
        val arr = ByteArray(bytes.size) { bytes[it].toByte() }
        viewModelScope.launch(Dispatchers.IO) {
            runCatching { os.write(arr); os.flush() }
        }
    }

    /** Turn a typed character into its Ctrl-modified control code (Ctrl-C, …). */
    fun sendCtrl(text: String) {
        val ch = text.firstOrNull() ?: return
        val code = ch.uppercaseChar().code
        val ctrl = when {
            code in 64..95 -> code - 64
            ch in 'a'..'z' -> ch.code - 96
            else -> code
        }
        sendBytes(ctrl and 0x7F)
    }

    fun disconnect() {
        viewModelScope.launch(Dispatchers.IO) { closeQuietly() }
        renderJob?.cancel()
        _status.value = TermStatus.Idle
    }

    private fun closeQuietly() {
        runCatching { session?.close() }
        runCatching { ssh?.disconnect() }
        session = null; ssh = null; outStream = null
    }

    override fun onCleared() {
        renderJob?.cancel()
        closeQuietly()
    }

    private companion object {
        const val COLS = 80
        const val ROWS = 24
    }
}
