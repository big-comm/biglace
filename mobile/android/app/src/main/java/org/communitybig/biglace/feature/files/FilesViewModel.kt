package org.communitybig.biglace.feature.files

import android.net.Uri
import android.provider.OpenableColumns
import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.SharedFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asSharedFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import net.schmizz.sshj.DefaultConfig
import net.schmizz.sshj.SSHClient
import net.schmizz.sshj.sftp.SFTPClient
import org.communitybig.biglace.AppContainer
import org.communitybig.biglace.R
import org.communitybig.biglace.core.ssh.AuthMode
import org.communitybig.biglace.core.ssh.ProbeResult
import org.communitybig.biglace.core.ssh.SshAuth
import org.communitybig.biglace.core.ssh.TofuHostKeyVerifier
import java.io.File
import java.util.UUID

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
    private val operations = Mutex()
    @Volatile private var disconnecting = false

    init {
        val cleanupBefore = System.currentTimeMillis()
        viewModelScope.launch(Dispatchers.IO) {
            stageDir().listFiles()
                ?.filter { it.lastModified() < cleanupBefore }
                ?.forEach { it.deleteRecursively() }
        }
    }

    fun connect(host: String, port: Int, user: String, password: String, mode: AuthMode) {
        if (_status.value == FilesStatus.Loading || _status.value == FilesStatus.Connected) return
        disconnecting = false
        _status.value = FilesStatus.Loading
        viewModelScope.launch(Dispatchers.IO) {
            try {
                val local = container.forward(host, port)
                val c = SSHClient(DefaultConfig())
                ssh = c
                c.addHostKeyVerifier(TofuHostKeyVerifier(host, container.secrets))
                c.connect("127.0.0.1", local)
                SshAuth.authenticate(
                    c, user, password, container.secrets.sshPrivateKey, mode, container.appContext,
                )
                if (password.isNotBlank() && mode != AuthMode.KEY) container.secrets.setSshPassword(host, password)
                if (user.isNotBlank()) container.secrets.setSshUser(host, user)
                val f = c.newSFTPClient()
                sftp = f
                loadInto(".")
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                _status.value = FilesStatus.Error(e.message ?: text(R.string.sftp_connection_failed))
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
                c.addHostKeyVerifier(TofuHostKeyVerifier(host, container.secrets))
                c.connect("127.0.0.1", local)
                val method = SshAuth.authenticate(
                    c, user, password, container.secrets.sshPrivateKey, mode, container.appContext,
                )
                if (password.isNotBlank() && mode != AuthMode.KEY) container.secrets.setSshPassword(host, password)
                if (user.isNotBlank()) container.secrets.setSshUser(host, user)
                _probe.value = ProbeResult(true, text(R.string.ssh_probe_success, user, host, method))
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                _probe.value = ProbeResult(false, e.message ?: text(R.string.ssh_connection_failed))
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
        viewModelScope.launch(Dispatchers.IO) {
            try {
                operations.withLock { loadInto(p) }
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                _events.tryEmit(FilesEvent.Message(e.message ?: text(R.string.files_listing_failed)))
            }
        }
    }

    private fun loadInto(p: String) {
        val f = sftp ?: return
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
    }

    // ── Operations ──────────────────────────────────────────────────────────

    fun rename(entry: RemoteFile, newName: String) = op(text(R.string.files_busy_renaming)) { f ->
        val clean = newName.trim()
        require(clean.isNotBlank() && '/' !in clean && clean.none(Char::isISOControl)) {
            text(R.string.files_invalid_name)
        }
        f.rename(child(entry.name), child(clean))
        msg(text(R.string.files_renamed_to, clean))
    }

    /** Move to another path. [dest] may be an absolute path or a directory. */
    fun move(entry: RemoteFile, dest: String) = op(text(R.string.files_busy_moving)) { f ->
        val target = resolveDest(dest.trim(), entry.name)
        require(target.none(Char::isISOControl)) { text(R.string.files_invalid_name) }
        f.rename(child(entry.name), target)
        msg(text(R.string.files_moved_to, target))
    }

    fun delete(entry: RemoteFile) = op(text(R.string.files_busy_deleting)) { f ->
        rmRecursive(f, child(entry.name), entry.isDir, 0)
        msg(text(R.string.files_deleted, entry.name))
    }

    fun mkdir(name: String) = op(text(R.string.files_busy_creating_folder)) { f ->
        val clean = name.trim()
        require(clean.isNotBlank() && '/' !in clean && clean.none(Char::isISOControl)) {
            text(R.string.files_invalid_name)
        }
        f.mkdir(child(clean))
        msg(text(R.string.files_folder_created))
    }

    fun openFile(entry: RemoteFile) = op(text(R.string.files_busy_downloading, entry.name)) { f ->
        val local = download(f, entry)
        _events.tryEmit(FilesEvent.OpenFile(local))
    }

    fun shareFile(entry: RemoteFile) = op(text(R.string.files_busy_downloading, entry.name)) { f ->
        val local = download(f, entry)
        _events.tryEmit(FilesEvent.ShareFile(local))
    }

    fun upload(uri: Uri) = op(text(R.string.files_busy_uploading)) { f ->
        val cr = container.appContext.contentResolver
        val name = sanitizeStagedFilename(displayName(uri), text(R.string.files_invalid_filename))
        val tmp = stageFile(name)
        try {
            cr.openInputStream(uri).use { input ->
                requireNotNull(input) { text(R.string.files_cannot_read_file) }
                tmp.outputStream().use { input.copyTo(it) }
            }
            f.put(tmp.absolutePath, child(name))
        } finally {
            tmp.parentFile?.deleteRecursively()
        }
        msg(text(R.string.files_uploaded, name))
    }

    private fun download(f: SFTPClient, entry: RemoteFile): File {
        val local = stageFile(
            sanitizeStagedFilename(entry.name, text(R.string.files_invalid_filename)),
        )
        f.get(child(entry.name), local.absolutePath)
        return local
    }

    private fun rmRecursive(f: SFTPClient, path: String, isDir: Boolean, depth: Int) {
        require(depth <= MAX_DELETE_DEPTH) { text(R.string.files_tree_too_deep) }
        if (isDir) {
            f.ls(path).filter { it.name != "." && it.name != ".." }
                .forEach { rmRecursive(f, it.path, it.isDirectory, depth + 1) }
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
        if (_busy.value != null) return
        _busy.value = label
        viewModelScope.launch(Dispatchers.IO) {
            try {
                operations.withLock { block(f) }
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                _events.tryEmit(FilesEvent.Message(e.message ?: text(R.string.files_operation_failed)))
            } finally {
                if (!disconnecting) {
                    try {
                        operations.withLock { loadInto(_path.value.ifBlank { "." }) }
                    } catch (e: CancellationException) {
                        throw e
                    } catch (e: Exception) {
                        _events.tryEmit(
                            FilesEvent.Message(e.message ?: text(R.string.files_listing_failed)),
                        )
                    }
                }
                _busy.value = null
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

    private fun stageFile(name: String): File {
        val dir = File(stageDir(), UUID.randomUUID().toString()).apply { mkdirs() }
        val file = File(dir, name)
        check(file.canonicalPath.startsWith(dir.canonicalPath + File.separator)) { "Invalid local filename" }
        return file
    }

    fun disconnect() {
        disconnecting = true
        viewModelScope.launch(Dispatchers.IO) {
            operations.withLock { closeQuietly() }
            _status.value = FilesStatus.Idle
            _entries.value = emptyList()
            _path.value = ""
        }
    }

    private fun closeQuietly() {
        runCatching { sftp?.close() }
        runCatching { ssh?.disconnect() }
        sftp = null; ssh = null
    }

    override fun onCleared() {
        closeQuietly()
    }

    private companion object {
        const val MAX_DELETE_DEPTH = 128
    }

    private fun text(id: Int, vararg args: Any): String = container.appContext.getString(id, *args)
}

internal fun sanitizeStagedFilename(input: String, invalidMessage: String = "Invalid filename"): String {
    val name = input.substringAfterLast('/').substringAfterLast('\\')
        .filter { !it.isISOControl() }
        .trim()
    require(name.isNotBlank() && name != "." && name != "..") { invalidMessage }
    val safe = name.take(180).trimEnd('.', ' ')
    require(safe.isNotBlank()) { invalidMessage }
    return safe
}
