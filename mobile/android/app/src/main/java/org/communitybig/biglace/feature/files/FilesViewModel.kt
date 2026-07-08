package org.communitybig.biglace.feature.files

import android.net.Uri
import android.provider.OpenableColumns
import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.SharedFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asSharedFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import net.schmizz.sshj.DefaultConfig
import net.schmizz.sshj.SSHClient
import net.schmizz.sshj.sftp.SFTPClient
import net.schmizz.sshj.transport.verification.PromiscuousVerifier
import org.communitybig.biglace.AppContainer
import org.communitybig.biglace.core.ssh.AuthMode
import org.communitybig.biglace.core.ssh.ProbeResult
import org.communitybig.biglace.core.ssh.SshAuth
import java.io.File

sealed interface FilesStatus {
    data object Idle : FilesStatus
    data object Loading : FilesStatus
    data object Connected : FilesStatus
    data class Error(val message: String) : FilesStatus
}

data class RemoteFile(val name: String, val isDir: Boolean, val size: Long)

/** One-shot signals for the UI (snackbars, opening/sharing a downloaded file). */
sealed interface FilesEvent {
    data class Message(val text: String) : FilesEvent
    data class OpenFile(val file: File) : FilesEvent
    data class ShareFile(val file: File) : FilesEvent
}

/**
 * A real SFTP file manager over the tailnet (sshj + tsnet-forward). Lists and
 * navigates directories and supports the full set of operations: open a file on
 * the phone (download to cache + hand off to a viewer), share, rename, move,
 * delete (recursive), make directory, and upload from the phone.
 */
class FilesViewModel(private val container: AppContainer) : ViewModel() {

    private val _status = MutableStateFlow<FilesStatus>(FilesStatus.Idle)
    val status: StateFlow<FilesStatus> = _status.asStateFlow()

    private val _path = MutableStateFlow("")
    val path: StateFlow<String> = _path.asStateFlow()

    private val _entries = MutableStateFlow<List<RemoteFile>>(emptyList())
    val entries: StateFlow<List<RemoteFile>> = _entries.asStateFlow()

    /** Non-null while a blocking operation (download/upload/delete) is running. */
    private val _busy = MutableStateFlow<String?>(null)
    val busy: StateFlow<String?> = _busy.asStateFlow()

    private val _events = MutableSharedFlow<FilesEvent>(extraBufferCapacity = 16)
    val events: SharedFlow<FilesEvent> = _events.asSharedFlow()

    private val _testing = MutableStateFlow(false)
    val testing: StateFlow<Boolean> = _testing.asStateFlow()

    private val _probe = MutableStateFlow<ProbeResult?>(null)
    val probe: StateFlow<ProbeResult?> = _probe.asStateFlow()

    private var ssh: SSHClient? = null
    private var sftp: SFTPClient? = null

    fun connect(host: String, port: Int, user: String, password: String, mode: AuthMode) {
        if (_status.value == FilesStatus.Loading || _status.value == FilesStatus.Connected) return
        _status.value = FilesStatus.Loading
        viewModelScope.launch(Dispatchers.IO) {
            try {
                val local = container.forward(host, port)
                val c = SSHClient(DefaultConfig())
                c.addHostKeyVerifier(PromiscuousVerifier())
                c.connect("127.0.0.1", local)
                SshAuth.authenticate(c, user, password, container.secrets.sshPrivateKey, mode)
                if (password.isNotBlank() && mode != AuthMode.KEY) container.secrets.setSshPassword(host, password)
                if (user.isNotBlank()) container.secrets.setSshUser(host, user)
                val f = c.newSFTPClient()
                ssh = c; sftp = f
                loadInto(".")
            } catch (e: Exception) {
                _status.value = FilesStatus.Error(e.message ?: "SFTP connection failed")
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

    fun dismissError() { if (_status.value is FilesStatus.Error) _status.value = FilesStatus.Idle }

    fun open(entry: RemoteFile) {
        if (entry.isDir) list(child(entry.name)) else openFile(entry)
    }

    fun up() {
        val parent = _path.value.trimEnd('/').substringBeforeLast('/', "/").ifEmpty { "/" }
        list(parent)
    }

    fun refresh() = list(_path.value.ifBlank { "." })

    private fun list(p: String) {
        viewModelScope.launch(Dispatchers.IO) { loadInto(p) }
    }

    private fun loadInto(p: String) {
        val f = sftp ?: return
        try {
            val real = f.canonicalize(p)
            val listing = f.ls(real)
                .asSequence()
                .filter { it.name != "." && it.name != ".." }
                .map { RemoteFile(it.name, it.isDirectory, it.attributes.size) }
                .sortedWith(compareByDescending<RemoteFile> { it.isDir }.thenBy { it.name.lowercase() })
                .toList()
            _path.value = real
            _entries.value = listing
            _status.value = FilesStatus.Connected
        } catch (e: Exception) {
            _status.value = FilesStatus.Error(e.message ?: "listing failed")
        }
    }

    // ── Operations ──────────────────────────────────────────────────────────

    fun rename(entry: RemoteFile, newName: String) = op("Renaming…") { f ->
        val clean = newName.trim()
        require(clean.isNotBlank() && '/' !in clean) { "Invalid name" }
        f.rename(child(entry.name), child(clean))
        msg("Renamed to $clean")
    }

    /** Move to another path. [dest] may be an absolute path or a directory. */
    fun move(entry: RemoteFile, dest: String) = op("Moving…") { f ->
        val target = resolveDest(dest.trim(), entry.name)
        f.rename(child(entry.name), target)
        msg("Moved to $target")
    }

    fun delete(entry: RemoteFile) = op("Deleting…") { f ->
        rmRecursive(f, child(entry.name), entry.isDir)
        msg("Deleted ${entry.name}")
    }

    fun mkdir(name: String) = op("Creating folder…") { f ->
        val clean = name.trim()
        require(clean.isNotBlank() && '/' !in clean) { "Invalid name" }
        f.mkdir(child(clean))
        msg("Folder created")
    }

    fun openFile(entry: RemoteFile) = op("Downloading ${entry.name}…") { f ->
        val local = download(f, entry)
        _events.tryEmit(FilesEvent.OpenFile(local))
    }

    fun shareFile(entry: RemoteFile) = op("Downloading ${entry.name}…") { f ->
        val local = download(f, entry)
        _events.tryEmit(FilesEvent.ShareFile(local))
    }

    fun upload(uri: Uri) = op("Uploading…") { f ->
        val cr = container.appContext.contentResolver
        val name = displayName(uri)
        val tmp = File(stageDir(), name)
        cr.openInputStream(uri).use { input ->
            requireNotNull(input) { "Can't read picked file" }
            tmp.outputStream().use { input.copyTo(it) }
        }
        f.put(tmp.absolutePath, child(name))
        tmp.delete()
        msg("Uploaded $name")
    }

    private fun download(f: SFTPClient, entry: RemoteFile): File {
        val local = File(stageDir(), entry.name)
        f.get(child(entry.name), local.absolutePath)
        return local
    }

    private fun rmRecursive(f: SFTPClient, path: String, isDir: Boolean) {
        if (isDir) {
            f.ls(path).filter { it.name != "." && it.name != ".." }
                .forEach { rmRecursive(f, it.path, it.isDirectory) }
            f.rmdir(path)
        } else {
            f.rm(path)
        }
    }

    /**
     * Run a blocking SFTP operation off the main thread, showing a [busy] label,
     * surfacing failures as a snackbar, and refreshing the listing afterwards.
     */
    private fun op(label: String, block: (SFTPClient) -> Unit) {
        val f = sftp ?: return
        viewModelScope.launch(Dispatchers.IO) {
            _busy.value = label
            try {
                block(f)
            } catch (e: Exception) {
                _events.tryEmit(FilesEvent.Message(e.message ?: "Operation failed"))
            } finally {
                _busy.value = null
                runCatching { loadInto(_path.value.ifBlank { "." }) }
            }
        }
    }

    private fun msg(text: String) { _events.tryEmit(FilesEvent.Message(text)) }

    private fun child(name: String): String = _path.value.trimEnd('/') + "/" + name

    private fun resolveDest(dest: String, name: String): String = when {
        dest.isBlank() -> child(name)
        dest.endsWith("/") -> dest + name          // into a directory
        dest.startsWith("/") -> dest               // absolute full path
        else -> child(dest)                        // relative to current dir
    }

    private fun displayName(uri: Uri): String {
        val cr = container.appContext.contentResolver
        cr.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)?.use { c ->
            if (c.moveToFirst()) {
                val i = c.getColumnIndex(OpenableColumns.DISPLAY_NAME)
                if (i >= 0) c.getString(i)?.let { return it }
            }
        }
        return uri.lastPathSegment?.substringAfterLast('/') ?: "upload.bin"
    }

    private fun stageDir(): File = File(container.appContext.cacheDir, "sftp").apply { mkdirs() }

    fun disconnect() {
        viewModelScope.launch(Dispatchers.IO) { closeQuietly() }
        _status.value = FilesStatus.Idle
        _entries.value = emptyList()
        _path.value = ""
    }

    private fun closeQuietly() {
        runCatching { sftp?.close() }
        runCatching { ssh?.disconnect() }
        sftp = null; ssh = null
    }

    override fun onCleared() {
        closeQuietly()
    }
}
