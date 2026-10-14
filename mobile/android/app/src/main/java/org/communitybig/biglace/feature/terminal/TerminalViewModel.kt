package org.communitybig.biglace.feature.terminal

import androidx.compose.runtime.Immutable
import androidx.compose.ui.text.AnnotatedString
import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.launch
import net.schmizz.sshj.SSHClient
import net.schmizz.sshj.connection.channel.direct.Session
import net.schmizz.sshj.connection.channel.direct.SessionChannel
import org.communitybig.biglace.AppContainer
import org.communitybig.biglace.R
import org.communitybig.biglace.core.ssh.AuthMode
import org.communitybig.biglace.core.ssh.HostKeyChangedException
import org.communitybig.biglace.core.ssh.ProbeResult
import org.communitybig.biglace.core.ssh.SshConnector
import java.io.InputStreamReader

sealed interface TermStatus {
    data object Idle : TermStatus
    data object Connecting : TermStatus
    data object Connected : TermStatus

    /** [hostKeyHost] is set when the failure is a changed SSH host key. */
    data class Error(val message: String, val hostKeyHost: String? = null) : TermStatus
}

/**
 * One rendered terminal frame, a line per entry. Lines that did not change keep
 * their instance between frames, so the UI only re-lays-out what was repainted.
 */
@Immutable
class TerminalFrame(val lines: List<AnnotatedString>)

/**
 * A real interactive SSH shell over the tailnet. [AppContainer.forward] opens a
 * local port tunnelling to the peer through the embedded tsnet engine; sshj
 * drives a PTY-backed shell on it, and bytes flow through [TerminalEmulator]
 * (a grid VT emulator with colours). Auth follows the chosen [AuthMode], and a
 * separate [probe] tests credentials without opening a shell.
 */
class TerminalViewModel(private val container: AppContainer) : ViewModel() {

    private val emulator = TerminalEmulator()

    private val _frame = MutableStateFlow(TerminalFrame(emptyList()))
    val frame: StateFlow<TerminalFrame> = _frame.asStateFlow()

    private val _status = MutableStateFlow<TermStatus>(TermStatus.Idle)
    val status: StateFlow<TermStatus> = _status.asStateFlow()

    private val _testing = MutableStateFlow(false)
    val testing: StateFlow<Boolean> = _testing.asStateFlow()

    private val _probe = MutableStateFlow<ProbeResult?>(null)
    val probe: StateFlow<ProbeResult?> = _probe.asStateFlow()

    /** Everything one connection attempt owns, so attempts can't close each other. */
    private class Link {
        @Volatile var client: SSHClient? = null
        @Volatile var session: Session? = null
        val input = Channel<ByteArray>(Channel.BUFFERED)

        fun close() {
            input.close()
            runCatching { session?.close() }
            runCatching { client?.disconnect() }
        }
    }

    @Volatile private var link: Link? = null

    /** Bumped by every connect/disconnect; a stale attempt must not touch the UI. */
    @Volatile private var generation = 0

    private val renderRequests = Channel<Unit>(Channel.CONFLATED)

    init {
        emulator.onReply = { reply -> link?.input?.trySend(reply.toByteArray(Charsets.UTF_8)) }
        startRenderPump()
    }

    fun connect(host: String, port: Int, user: String, password: String, mode: AuthMode) {
        if (_status.value == TermStatus.Connecting || _status.value == TermStatus.Connected) return
        val attempt = ++generation
        _status.value = TermStatus.Connecting
        emulator.clear()
        _frame.value = TerminalFrame(emptyList())
        val current = Link()
        link = current
        viewModelScope.launch(Dispatchers.IO) {
            try {
                val ssh = SshConnector.connect(container, host, port, user, password, mode) {
                    current.client = it
                    // Disconnected while the tunnel was being opened.
                    if (generation != attempt) throw CancellationException("superseded")
                }
                val session = ssh.client.startSession()
                current.session = session
                session.allocatePTY("xterm-256color", emulator.cols, emulator.rows, 0, 0, emptyMap())
                val shell = session.startShell()
                val writer = launch {
                    try {
                        for (bytes in current.input) {
                            shell.outputStream.write(bytes)
                            shell.outputStream.flush()
                        }
                    } catch (e: CancellationException) {
                        throw e
                    } catch (_: Exception) {
                        // The transport died mid-keystroke; the read loop below
                        // sees the same failure and reports it.
                    }
                }
                emulator.feed(
                    "\u001B[90m${text(R.string.ssh_authenticated_via, ssh.authMethod)}\u001B[0m\r\n",
                )
                requestRender()
                if (generation == attempt) _status.value = TermStatus.Connected

                val buf = CharArray(8192)
                val reader = InputStreamReader(shell.inputStream, Charsets.UTF_8)
                while (true) {
                    val n = reader.read(buf)
                    if (n < 0) break
                    emulator.feed(String(buf, 0, n))
                    requestRender()
                }
                writer.cancel()
                if (generation == attempt) _status.value = TermStatus.Idle
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                if (generation == attempt) {
                    _status.value = TermStatus.Error(
                        e.message ?: text(R.string.ssh_connection_failed),
                        (e as? HostKeyChangedException)?.host,
                    )
                }
            } finally {
                current.close()
                if (link === current) link = null
            }
        }
    }

    /** Test credentials only (open transport, authenticate, close) and report. */
    fun probe(host: String, port: Int, user: String, password: String, mode: AuthMode) {
        if (_testing.value) return
        _testing.value = true
        viewModelScope.launch(Dispatchers.IO) {
            try {
                val ssh = SshConnector.connect(container, host, port, user, password, mode)
                runCatching { ssh.client.disconnect() }
                _probe.value = ProbeResult(true, text(R.string.ssh_probe_success, user, host, ssh.authMethod))
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                _probe.value = ProbeResult(false, e.message ?: text(R.string.ssh_connection_failed))
            } finally {
                _testing.value = false
            }
        }
    }

    fun clearProbe() { _probe.value = null }

    fun dismissError() { if (_status.value is TermStatus.Error) _status.value = TermStatus.Idle }

    /** The user accepted a changed host key: drop the old one so the next connect trusts anew. */
    fun forgetHostKey(host: String) {
        container.secrets.forgetSshHostFingerprint(host)
        dismissError()
    }

    private fun requestRender() { renderRequests.trySend(Unit) }

    /**
     * Turns emulator changes into frames, off the UI thread and at most once per
     * [RENDER_FRAME_MS]. It sleeps on a channel (no polling) and does no work at
     * all while nothing is displaying this session — a background tab running a
     * busy TUI costs nothing until it is looked at again.
     */
    private fun startRenderPump() {
        viewModelScope.launch(Dispatchers.Default) {
            var held = 0
            for (request in renderRequests) {
                _frame.subscriptionCount.first { it > 0 }
                // Mid-repaint (synchronized output): wait for the app to finish,
                // but never hold the screen hostage if it forgets to say so.
                if (emulator.synchronizedOutput && held < MAX_HELD_FRAMES) {
                    held++
                    requestRender()
                } else {
                    held = 0
                    _frame.value = TerminalFrame(emulator.snapshot(SCROLLBACK_ROWS))
                }
                delay(RENDER_FRAME_MS)
            }
        }
    }

    private fun send(bytes: ByteArray) { link?.input?.trySend(bytes) }

    /** Send raw text straight to the shell (used for typed characters). */
    fun sendRaw(text: String) = send(text.toByteArray(Charsets.UTF_8))

    fun sendBytes(vararg bytes: Int) = send(ByteArray(bytes.size) { bytes[it].toByte() })

    /** Turn a typed character into its Ctrl-modified control code (Ctrl-C, …). */
    fun sendCtrl(text: String) {
        val ch = text.firstOrNull() ?: return
        val code = ch.uppercaseChar().code
        val ctrl = when {
            ch == ' ' -> 0
            code in 64..95 -> code - 64
            ch in 'a'..'z' -> ch.code - 96
            else -> code
        }
        sendBytes(ctrl and 0x7F)
    }

    /** Arrow key ('A' up, 'B' down, 'C' right, 'D' left) in the mode the app asked for. */
    fun sendArrow(direction: Char) {
        sendRaw(if (emulator.applicationCursorKeys) "\u001BO$direction" else "\u001B[$direction")
    }

    /** Paste text, bracketed when the shell supports it so it isn't run line by line. */
    fun paste(text: String) {
        if (text.isEmpty()) return
        val body = text.replace("\r\n", "\r").replace('\n', '\r')
        sendRaw(if (emulator.bracketedPaste) "\u001B[200~$body\u001B[201~" else body)
    }

    /** Everything currently in the buffer (scrollback + screen) as plain text. */
    fun screenText(): String = emulator.render(Int.MAX_VALUE).text

    fun resize(rows: Int, cols: Int) {
        emulator.resize(rows, cols)
        requestRender()
        val channel = link?.session as? SessionChannel ?: return
        viewModelScope.launch(Dispatchers.IO) {
            runCatching { channel.changeWindowDimensions(emulator.cols, emulator.rows, 0, 0) }
        }
    }

    fun disconnect() {
        generation++
        _status.value = TermStatus.Idle
        closeLink()
    }

    // Closing sockets must not run on the main thread, and must still happen
    // after this ViewModel's own scope is cancelled (tab closed, app exiting).
    private fun closeLink() {
        val current = link ?: return
        link = null
        container.ioScope.launch { current.close() }
    }

    override fun onCleared() {
        generation++
        emulator.onReply = null
        closeLink()
    }

    private fun text(id: Int, vararg args: Any): String = container.appContext.getString(id, *args)

    private companion object {
        const val SCROLLBACK_ROWS = 800
        const val RENDER_FRAME_MS = 33L
        const val MAX_HELD_FRAMES = 6
    }
}
