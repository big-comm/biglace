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
import net.schmizz.sshj.SSHClient
import net.schmizz.sshj.common.StreamCopier
import net.schmizz.sshj.sftp.FileMode
import net.schmizz.sshj.sftp.SFTPClient
import net.schmizz.sshj.xfer.TransferListener
import org.communitybig.biglace.AppContainer
import org.communitybig.biglace.R
import org.communitybig.biglace.core.ssh.AuthMode
import org.communitybig.biglace.core.ssh.HostKeyChangedException
import org.communitybig.biglace.core.ssh.ProbeResult
import org.communitybig.biglace.core.ssh.SshConnector
import java.io.File
import java.io.IOException
import java.util.UUID
import java.util.concurrent.atomic.AtomicBoolean

sealed interface FilesStatus {
    data object Idle : FilesStatus
    data object Loading : FilesStatus
    data object Connected : FilesStatus

    /** [hostKeyHost] is set when the failure is a changed SSH host key. */
    data class Error(val message: String, val hostKeyHost: String? = null) : FilesStatus
}

/**
 * [isDir] follows symlinks (so a linked directory can be entered); [isLink]
 * records that the entry itself is a link, which is what delete must act on.
 */
data class RemoteFile(val name: String, val isDir: Boolean, val size: Long, val isLink: Boolean = false)

/** A blocking operation in progress; transfers can be cancelled, quick ops cannot. */
data class FilesBusy(val label: String, val cancellable: Boolean)

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
    private val _busy = MutableStateFlow<FilesBusy?>(null)
    val busy: StateFlow<FilesBusy?> = _busy.asStateFlow()

    private val _events = MutableSharedFlow<FilesEvent>(extraBufferCapacity = 16)
    val events: SharedFlow<FilesEvent> = _events.asSharedFlow()

    private val _testing = MutableStateFlow(false)
    val testing: StateFlow<Boolean> = _testing.asStateFlow()

    private val _probe = MutableStateFlow<ProbeResult?>(null)
    val probe: StateFlow<ProbeResult?> = _probe.asStateFlow()

    @Volatile private var ssh: SSHClient? = null
    @Volatile private var sftp: SFTPClient? = null
    private val operations = Mutex()
    @Volatile private var disconnecting = false
    @Volatile private var cancelRequested = false

    /** Bumped by every connect/disconnect; a stale attempt must not touch state. */
    @Volatile private var generation = 0

    init {
        // Staged downloads from a previous run are garbage. Once per process:
        // doing it per ViewModel would delete files another tab just handed to
        // a viewer app.
        if (stageCleaned.compareAndSet(false, true)) {
            val cleanupBefore = System.currentTimeMillis()
            container.ioScope.launch {
                stageDir().listFiles()
                    ?.filter { it.lastModified() < cleanupBefore }
                    ?.forEach { it.deleteRecursively() }
            }
        }
    }

    fun connect(host: String, port: Int, user: String, password: String, mode: AuthMode) {
        if (_status.value == FilesStatus.Loading || _status.value == FilesStatus.Connected) return
        val attempt = ++generation
        disconnecting = false
        _status.value = FilesStatus.Loading
        viewModelScope.launch(Dispatchers.IO) {
            var client: SSHClient? = null
            try {
                val connection = SshConnector.connect(container, host, port, user, password, mode) {
                    client = it
                    // Disconnected (tab closed) while the tunnel was being opened.
                    if (generation != attempt) throw CancellationException("superseded")
                    ssh = it
                }
                val f = connection.client.newSFTPClient()
                f.fileTransfer.transferListener = TransferProgress()
                if (generation != attempt) throw CancellationException("superseded")
                sftp = f
                loadInto(".")
            } catch (e: CancellationException) {
                runCatching { client?.disconnect() }
                throw e
            } catch (e: Exception) {
                runCatching { client?.disconnect() }
                if (generation == attempt) {
                    closeQuietly()
                    _status.value = FilesStatus.Error(
                        e.message ?: text(R.string.sftp_connection_failed),
                        (e as? HostKeyChangedException)?.host,
                    )
                }
            }
        }
    }

    /** Test credentials only (open transport, authenticate, close) and report. */
    fun probe(host: String, port: Int, user: String, password: String, mode: AuthMode) {
        if (_testing.value) return
        _testing.value = true
        viewModelScope.launch(Dispatchers.IO) {
            try {
                val connection = SshConnector.connect(container, host, port, user, password, mode)
                runCatching { connection.client.disconnect() }
                _probe.value = ProbeResult(true, text(R.string.ssh_probe_success, user, host, connection.authMethod))
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

    fun dismissError() { if (_status.value is FilesStatus.Error) _status.value = FilesStatus.Idle }

    /** The user accepted a changed host key: drop the old one so the next connect trusts anew. */
    fun forgetHostKey(host: String) {
        container.secrets.forgetSshHostFingerprint(host)
        dismissError()
    }

    fun open(entry: RemoteFile) {
        if (entry.isDir) list(child(entry.name)) else openFile(entry)
    }

    /** Whether [up] would go anywhere (false at the filesystem root). */
    fun canGoUp(): Boolean = _path.value.trimEnd('/').isNotEmpty()

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
                if (!disconnecting) {
                    _events.tryEmit(FilesEvent.Message(e.message ?: text(R.string.files_listing_failed)))
                }
            }
        }
    }

    private fun loadInto(p: String) {
        val f = sftp ?: return
        val real = f.canonicalize(p)
        val listing = f.ls(real)
            .asSequence()
            .filter { it.name != "." && it.name != ".." }
            .map { info ->
                // A directory listing describes links themselves; ask what a
                // link points at so linked directories (/bin, /lib …) open as
                // directories instead of being "downloaded" as files.
                val isLink = info.attributes.type == FileMode.Type.SYMLINK
                val target = if (isLink) runCatching { f.stat(info.path) }.getOrNull() else null
                val attrs = target ?: info.attributes
                RemoteFile(info.name, attrs.type == FileMode.Type.DIRECTORY, attrs.size, isLink)
            }
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
        // A link is removed as a link — never by emptying what it points at.
        rmRecursive(f, child(entry.name), entry.isDir && !entry.isLink, 0)
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

    fun openFile(entry: RemoteFile) = transfer(text(R.string.files_busy_downloading, entry.name)) { f ->
        val local = download(f, entry)
        _events.tryEmit(FilesEvent.OpenFile(local))
    }

    fun shareFile(entry: RemoteFile) = transfer(text(R.string.files_busy_downloading, entry.name)) { f ->
        val local = download(f, entry)
        _events.tryEmit(FilesEvent.ShareFile(local))
    }

    fun upload(uri: Uri) = transfer(text(R.string.files_busy_uploading)) { f ->
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

    /** Stop the running download/upload; the session stays connected. */
    fun cancelTransfer() { cancelRequested = true }

    private fun download(f: SFTPClient, entry: RemoteFile): File {
        val remote = child(entry.name)
        // sshj's get() follows links and recurses into directories, so check
        // what is really there before pulling it into the phone's cache.
        val attrs = f.stat(remote)
        require(attrs.type != FileMode.Type.DIRECTORY) {
            text(R.string.files_cannot_open_directory, entry.name)
        }
        require(attrs.size < stageDir().usableSpace - FREE_SPACE_RESERVE) {
            text(R.string.files_not_enough_space, entry.name)
        }
        val local = stageFile(
            sanitizeStagedFilename(entry.name, text(R.string.files_invalid_filename)),
        )
        try {
            f.get(remote, local.absolutePath)
        } catch (e: Exception) {
            local.parentFile?.deleteRecursively()
            throw e
        }
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

    /** Reports transfer progress into [busy] and aborts the copy on cancel. */
    private inner class TransferProgress : TransferListener {
        override fun directory(name: String): TransferListener = this

        override fun file(name: String, size: Long): StreamCopier.Listener {
            val label = _busy.value?.label.orEmpty()
            var shown = -1
            return StreamCopier.Listener { transferred ->
                if (cancelRequested) throw IOException(text(R.string.files_cancelled))
                if (size > 0) {
                    val percent = (transferred * 100 / size).toInt().coerceIn(0, 100)
                    if (percent != shown) {
                        shown = percent
                        _busy.value = FilesBusy(text(R.string.files_progress, label, percent), true)
                    }
                }
            }
        }
    }

    private fun transfer(label: String, block: (SFTPClient) -> Unit) = op(label, cancellable = true, block)

    /**
     * Run a blocking SFTP operation off the main thread, showing a [busy] label,
     * surfacing failures as a snackbar, and refreshing the listing afterwards.
     */
    private fun op(label: String, cancellable: Boolean = false, block: (SFTPClient) -> Unit) {
        val f = sftp ?: return
        if (_busy.value != null) return
        cancelRequested = false
        _busy.value = FilesBusy(label, cancellable)
        viewModelScope.launch(Dispatchers.IO) {
            // The busy flag is the only thing `finally` does: rethrowing a
            // cancellation from inside it (as the refresh used to) skipped the
            // reset and left the screen stuck behind the busy overlay.
            try {
                try {
                    operations.withLock { block(f) }
                } catch (e: CancellationException) {
                    throw e
                } catch (e: Exception) {
                    if (!disconnecting) {
                        _events.tryEmit(FilesEvent.Message(e.message ?: text(R.string.files_operation_failed)))
                    }
                }
                if (!disconnecting) refreshAfterOperation()
            } finally {
                _busy.value = null
            }
        }
    }

    /** Re-list the current directory so the operation's effect shows up. */
    private suspend fun refreshAfterOperation() {
        try {
            operations.withLock { loadInto(_path.value.ifBlank { "." }) }
        } catch (e: CancellationException) {
            throw e
        } catch (e: Exception) {
            _events.tryEmit(FilesEvent.Message(e.message ?: text(R.string.files_listing_failed)))
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

    /**
     * Drops the connection right away. It deliberately does not wait for a
     * running operation: closing the transport is what makes a long transfer
     * stop, instead of the disconnect queueing up behind it.
     */
    fun disconnect() {
        generation++
        disconnecting = true
        cancelRequested = true
        _status.value = FilesStatus.Idle
        _entries.value = emptyList()
        _path.value = ""
        closeQuietly()
    }

    // Closing sockets must not run on the main thread, and must still happen
    // after this ViewModel's own scope is cancelled (tab closed, app exiting).
    private fun closeQuietly() {
        val client = ssh
        val f = sftp
        ssh = null
        sftp = null
        if (client == null && f == null) return
        container.ioScope.launch {
            runCatching { client?.disconnect() }
            runCatching { f?.close() }
        }
    }

    override fun onCleared() {
        generation++
        disconnecting = true
        cancelRequested = true
        closeQuietly()
    }

    private companion object {
        const val MAX_DELETE_DEPTH = 128
        const val FREE_SPACE_RESERVE = 64L * 1024 * 1024
        val stageCleaned = AtomicBoolean(false)
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
