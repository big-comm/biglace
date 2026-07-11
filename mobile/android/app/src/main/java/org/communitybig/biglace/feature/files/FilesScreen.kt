package org.communitybig.biglace.feature.files

import android.content.Context
import android.content.ClipData
import android.content.Intent
import android.webkit.MimeTypeMap
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.MoreVert
import androidx.compose.material.icons.filled.Close
import androidx.compose.material.icons.filled.Refresh
import androidx.compose.material.icons.filled.Search
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.derivedStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.saveable.rememberSaveableStateHolder
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import androidx.core.content.FileProvider
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.viewmodel.compose.viewModel
import androidx.lifecycle.viewmodel.initializer
import androidx.lifecycle.viewmodel.viewModelFactory
import org.communitybig.biglace.AppContainer
import org.communitybig.biglace.R
import org.communitybig.biglace.core.ssh.AuthMode
import org.communitybig.biglace.core.ssh.SshSessionTab
import org.communitybig.biglace.core.ssh.SshSessionTabsViewModel
import org.communitybig.biglace.feature.terminal.ConnectForm
import org.communitybig.biglace.ui.AuthDialog
import org.communitybig.biglace.ui.SessionTabs
import java.io.File
import java.util.Locale

internal enum class FileSort { NAME, SIZE, TYPE }

internal fun filterAndSortEntries(
    entries: List<RemoteFile>,
    query: String,
    sort: FileSort,
    ascending: Boolean,
): List<RemoteFile> {
    val filtered = entries.filter { it.name.contains(query.trim(), ignoreCase = true) }
    val comparator = when (sort) {
        FileSort.NAME -> compareBy<RemoteFile> { it.name.lowercase(Locale.ROOT) }
        FileSort.SIZE -> compareBy<RemoteFile> { it.size }.thenBy { it.name.lowercase(Locale.ROOT) }
        FileSort.TYPE -> compareBy<RemoteFile> {
            it.name.substringAfterLast('.', "").lowercase(Locale.ROOT)
        }.thenBy { it.name.lowercase(Locale.ROOT) }
    }
    return filtered.sortedWith(
        compareByDescending<RemoteFile> { it.isDir }
            .then(if (ascending) comparator else comparator.reversed()),
    )
}

@Composable
fun FilesScreen(container: AppContainer, modifier: Modifier = Modifier) {
    val tabsVm: SshSessionTabsViewModel = viewModel(key = "files-tabs")
    val tabs by tabsVm.tabs.collectAsStateWithLifecycle()
    val activeId by tabsVm.activeId.collectAsStateWithLifecycle()
    val pending by container.pendingTarget.collectAsStateWithLifecycle()
    val stateHolder = rememberSaveableStateHolder()

    LaunchedEffect(pending) {
        pending?.takeIf { it.wantFiles }?.let { target ->
            tabsVm.openPeer(
                target.host,
                container.secrets.sshUser(target.host) ?: target.user,
                container.secrets.sshPassword(target.host).orEmpty(),
            )
            container.pendingTarget.value = null
        }
    }

    val active = tabs.firstOrNull { it.id == activeId } ?: return
    val vm: FilesViewModel = viewModel(
        key = "files-session-${active.id}",
        factory = viewModelFactory { initializer { FilesViewModel(container) } },
    )

    Column(modifier.fillMaxSize()) {
        SessionTabs(
            tabs = tabs,
            activeId = active.id,
            onSelect = tabsVm::select,
            onAdd = tabsVm::add,
            onCloseActive = {
                vm.disconnect()
                stateHolder.removeState(active.id)
                tabsVm.close(active.id)
            },
        )
        stateHolder.SaveableStateProvider(active.id) {
            FilesSessionScreen(
                container = container,
                tab = active,
                vm = vm,
                onUpdate = { transform -> tabsVm.update(active.id, transform) },
                modifier = Modifier.weight(1f),
            )
        }
    }
}

@Composable
private fun FilesSessionScreen(
    container: AppContainer,
    tab: SshSessionTab,
    vm: FilesViewModel,
    onUpdate: ((SshSessionTab) -> SshSessionTab) -> Unit,
    modifier: Modifier = Modifier,
) {
    val status by vm.status.collectAsStateWithLifecycle()
    val path by vm.path.collectAsStateWithLifecycle()
    val entries by vm.entries.collectAsStateWithLifecycle()
    val busy by vm.busy.collectAsStateWithLifecycle()
    val testing by vm.testing.collectAsStateWithLifecycle()
    val probe by vm.probe.collectAsStateWithLifecycle()
    val context = androidx.compose.ui.platform.LocalContext.current
    val snackbar = remember { SnackbarHostState() }
    val noViewerMessage = stringResource(R.string.files_no_viewer)

    // Dialog targets.
    var renameTarget by remember { mutableStateOf<RemoteFile?>(null) }
    var moveTarget by remember { mutableStateOf<RemoteFile?>(null) }
    var deleteTarget by remember { mutableStateOf<RemoteFile?>(null) }
    var showMkdir by remember { mutableStateOf(false) }
    var query by rememberSaveable { mutableStateOf("") }
    var sort by rememberSaveable { mutableStateOf(FileSort.NAME) }
    var ascending by rememberSaveable { mutableStateOf(true) }
    val visibleEntries by remember(entries, query, sort, ascending) {
        derivedStateOf {
            filterAndSortEntries(entries, query, sort, ascending)
        }
    }

    val uploadPicker = rememberLauncherForActivityResult(ActivityResultContracts.GetContent()) { uri ->
        uri?.let { vm.upload(it) }
    }

    LaunchedEffect(tab.host) {
        if (tab.host.isNotBlank() && tab.password.isBlank()) {
            container.secrets.sshPassword(tab.host)?.let { saved ->
                onUpdate { it.copy(password = saved) }
            }
        }
    }

    LaunchedEffect(Unit) {
        vm.events.collect { event ->
            when (event) {
                is FilesEvent.Message -> snackbar.showSnackbar(event.text)
                is FilesEvent.OpenFile -> if (!openFile(context, event.file, share = false)) {
                    snackbar.showSnackbar(noViewerMessage)
                }
                is FilesEvent.ShareFile -> if (!openFile(context, event.file, share = true)) {
                    snackbar.showSnackbar(noViewerMessage)
                }
            }
        }
    }

    if (status == FilesStatus.Connected) {
        Scaffold(
            modifier = modifier,
            snackbarHost = { SnackbarHost(snackbar) },
        ) { pad ->
            Box(Modifier.fillMaxSize().padding(pad)) {
                Column(Modifier.fillMaxSize()) {
                    Surface(color = MaterialTheme.colorScheme.surfaceVariant) {
                        Row(
                            Modifier.fillMaxWidth().padding(horizontal = 8.dp, vertical = 4.dp),
                            verticalAlignment = Alignment.CenterVertically,
                        ) {
                            TextButton(onClick = { vm.up() }) { Text("⬆") }
                            Text(
                                path, style = MaterialTheme.typography.bodySmall,
                                fontFamily = FontFamily.Monospace, modifier = Modifier.weight(1f),
                            )
                            IconButton(onClick = { vm.refresh() }) {
                                Icon(Icons.Filled.Refresh, contentDescription = stringResource(R.string.action_refresh))
                            }
                            ToolbarMenu(
                                onNewFolder = { showMkdir = true },
                                onUpload = { uploadPicker.launch("*/*") },
                                onDisconnect = { vm.disconnect() },
                            )
                        }
                    }
                    Row(
                        Modifier.fillMaxWidth().padding(horizontal = 8.dp, vertical = 6.dp),
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        OutlinedTextField(
                            value = query,
                            onValueChange = { query = it },
                            singleLine = true,
                            label = { Text(stringResource(R.string.files_search)) },
                            leadingIcon = { Icon(Icons.Filled.Search, contentDescription = null) },
                            trailingIcon = if (query.isNotEmpty()) {
                                {
                                    IconButton(onClick = { query = "" }) {
                                        Icon(Icons.Filled.Close, contentDescription = stringResource(R.string.files_clear_search))
                                    }
                                }
                            } else null,
                            modifier = Modifier.weight(1f),
                        )
                        SortMenu(
                            sort = sort,
                            ascending = ascending,
                            onSort = { sort = it },
                            onDirection = { ascending = !ascending },
                        )
                    }
                    LazyColumn(Modifier.fillMaxSize()) {
                        if (visibleEntries.isEmpty() && query.isNotBlank()) {
                            item {
                                Text(
                                    stringResource(R.string.files_no_search_results),
                                    modifier = Modifier.padding(16.dp),
                                    color = MaterialTheme.colorScheme.onSurface.copy(alpha = 0.6f),
                                )
                            }
                        }
                        items(visibleEntries, key = { it.name }) { entry ->
                            FileRow(
                                entry = entry,
                                onOpen = { vm.open(entry) },
                                onRename = { renameTarget = entry },
                                onMove = { moveTarget = entry },
                                onDelete = { deleteTarget = entry },
                                onShare = { vm.shareFile(entry) },
                            )
                            HorizontalDivider(color = MaterialTheme.colorScheme.onSurface.copy(alpha = 0.06f))
                        }
                    }
                }
                if (busy != null) {
                    Box(
                        Modifier.fillMaxSize().clickable(onClick = {})
                            .background(Color(0x99000000)),
                        contentAlignment = Alignment.Center,
                    ) {
                        Column(horizontalAlignment = Alignment.CenterHorizontally) {
                            CircularProgressIndicator()
                            Text(
                                busy ?: "",
                                color = Color.White,
                                style = MaterialTheme.typography.bodyMedium,
                                modifier = Modifier.padding(top = 12.dp),
                            )
                        }
                    }
                }
            }
        }
    } else {
        ConnectForm(
            iconRes = R.drawable.ic_folder,
            host = tab.host,
            onHost = { value ->
                onUpdate {
                    it.copy(
                        host = value,
                        password = if (value == it.host) it.password else "",
                        fromPeer = false,
                    )
                }
            },
            port = tab.port, onPort = { value -> onUpdate { it.copy(port = value) } },
            user = tab.user, onUser = { value -> onUpdate { it.copy(user = value) } },
            password = tab.password,
            onPassword = { value -> onUpdate { it.copy(password = value) } },
            fromPeer = tab.fromPeer,
            connecting = status == FilesStatus.Loading,
            testing = testing,
            mode = tab.mode, onMode = { value -> onUpdate { it.copy(mode = value) } },
            onConnect = {
                vm.connect(
                    tab.host.trim(), tab.port.toIntOrNull() ?: 22,
                    tab.user.trim(), tab.password, tab.mode,
                )
            },
            onTest = {
                vm.probe(
                    tab.host.trim(), tab.port.toIntOrNull() ?: 22,
                    tab.user.trim(), tab.password, tab.mode,
                )
            },
            modifier = modifier,
        )
    }

    (status as? FilesStatus.Error)?.let { err ->
        AuthDialog(
            ok = false,
            title = stringResource(R.string.ssh_auth_failed),
            message = err.message,
            confirmText = stringResource(R.string.action_ok),
            onDismiss = { vm.dismissError() },
        )
    }
    probe?.let { result ->
        AuthDialog(
            ok = result.ok,
            title = stringResource(if (result.ok) R.string.ssh_auth_ok else R.string.ssh_auth_failed),
            message = result.message,
            confirmText = stringResource(R.string.action_ok),
            onDismiss = { vm.clearProbe() },
        )
    }

    renameTarget?.let { target ->
        TextPromptDialog(
            title = stringResource(R.string.files_rename),
            initial = target.name,
            confirm = stringResource(R.string.action_save),
            onDismiss = { renameTarget = null },
            onConfirm = { vm.rename(target, it); renameTarget = null },
        )
    }
    moveTarget?.let { target ->
        TextPromptDialog(
            title = stringResource(R.string.files_move),
            initial = path.trimEnd('/') + "/" + target.name,
            confirm = stringResource(R.string.files_move),
            label = stringResource(R.string.files_move_hint),
            onDismiss = { moveTarget = null },
            onConfirm = { vm.move(target, it); moveTarget = null },
        )
    }
    if (showMkdir) {
        TextPromptDialog(
            title = stringResource(R.string.files_new_folder),
            initial = "",
            confirm = stringResource(R.string.files_create),
            onDismiss = { showMkdir = false },
            onConfirm = { vm.mkdir(it); showMkdir = false },
        )
    }
    deleteTarget?.let { target ->
        AlertDialog(
            onDismissRequest = { deleteTarget = null },
            title = { Text(stringResource(R.string.files_delete)) },
            text = { Text(stringResource(R.string.files_delete_confirm, target.name)) },
            confirmButton = {
                TextButton(onClick = { vm.delete(target); deleteTarget = null }) {
                    Text(stringResource(R.string.files_delete), color = MaterialTheme.colorScheme.error)
                }
            },
            dismissButton = {
                TextButton(onClick = { deleteTarget = null }) { Text(stringResource(R.string.dialog_cancel)) }
            },
        )
    }
}

@Composable
private fun SortMenu(
    sort: FileSort,
    ascending: Boolean,
    onSort: (FileSort) -> Unit,
    onDirection: () -> Unit,
) {
    var expanded by remember { mutableStateOf(false) }
    Box {
        IconButton(onClick = { expanded = true }) {
            Icon(
                painterResource(R.drawable.ic_sort),
                contentDescription = stringResource(R.string.files_sort),
            )
        }
        DropdownMenu(expanded = expanded, onDismissRequest = { expanded = false }) {
            FileSort.entries.forEach { option ->
                val label = when (option) {
                    FileSort.NAME -> R.string.files_sort_name
                    FileSort.SIZE -> R.string.files_sort_size
                    FileSort.TYPE -> R.string.files_sort_type
                }
                DropdownMenuItem(
                    text = { Text(stringResource(label)) },
                    onClick = { onSort(option); expanded = false },
                    leadingIcon = if (sort == option) {
                        { Text("✓") }
                    } else null,
                )
            }
            HorizontalDivider()
            DropdownMenuItem(
                text = {
                    Text(stringResource(if (ascending) R.string.files_sort_descending else R.string.files_sort_ascending))
                },
                onClick = { onDirection(); expanded = false },
            )
        }
    }
}

@Composable
private fun FileRow(
    entry: RemoteFile,
    onOpen: () -> Unit,
    onRename: () -> Unit,
    onMove: () -> Unit,
    onDelete: () -> Unit,
    onShare: () -> Unit,
) {
    val context = androidx.compose.ui.platform.LocalContext.current
    var menu by remember { mutableStateOf(false) }
    Row(
        Modifier.fillMaxWidth().clickable { onOpen() }.padding(horizontal = 12.dp, vertical = 12.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Icon(
            painterResource(if (entry.isDir) R.drawable.ic_folder else R.drawable.ic_file),
            contentDescription = null,
            tint = if (entry.isDir) MaterialTheme.colorScheme.primary
            else MaterialTheme.colorScheme.onSurface.copy(alpha = 0.55f),
        )
        Text(entry.name, modifier = Modifier.padding(start = 14.dp).weight(1f))
        if (!entry.isDir) {
            Text(
                android.text.format.Formatter.formatShortFileSize(context, entry.size),
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurface.copy(alpha = 0.5f),
                modifier = Modifier.padding(end = 4.dp),
            )
        }
        Box {
            IconButton(onClick = { menu = true }) {
                Icon(Icons.Filled.MoreVert, contentDescription = stringResource(R.string.files_actions))
            }
            DropdownMenu(expanded = menu, onDismissRequest = { menu = false }) {
                if (!entry.isDir) {
                    DropdownMenuItem(text = { Text(stringResource(R.string.files_open)) },
                        onClick = { menu = false; onOpen() })
                    DropdownMenuItem(text = { Text(stringResource(R.string.action_share)) },
                        onClick = { menu = false; onShare() })
                }
                DropdownMenuItem(text = { Text(stringResource(R.string.files_rename)) },
                    onClick = { menu = false; onRename() })
                DropdownMenuItem(text = { Text(stringResource(R.string.files_move)) },
                    onClick = { menu = false; onMove() })
                DropdownMenuItem(text = { Text(stringResource(R.string.files_delete)) },
                    onClick = { menu = false; onDelete() })
            }
        }
    }
}

@Composable
private fun ToolbarMenu(onNewFolder: () -> Unit, onUpload: () -> Unit, onDisconnect: () -> Unit) {
    var menu by remember { mutableStateOf(false) }
    Box {
        IconButton(onClick = { menu = true }) {
            Icon(Icons.Filled.MoreVert, contentDescription = stringResource(R.string.files_actions))
        }
        DropdownMenu(expanded = menu, onDismissRequest = { menu = false }) {
            DropdownMenuItem(text = { Text(stringResource(R.string.files_new_folder)) },
                onClick = { menu = false; onNewFolder() })
            DropdownMenuItem(text = { Text(stringResource(R.string.files_upload)) },
                onClick = { menu = false; onUpload() })
            DropdownMenuItem(text = { Text(stringResource(R.string.settings_disconnect)) },
                onClick = { menu = false; onDisconnect() })
        }
    }
}

@Composable
private fun TextPromptDialog(
    title: String,
    initial: String,
    confirm: String,
    label: String? = null,
    onDismiss: () -> Unit,
    onConfirm: (String) -> Unit,
) {
    var text by remember { mutableStateOf(initial) }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(title) },
        text = {
            OutlinedTextField(
                value = text, onValueChange = { text = it }, singleLine = true,
                label = label?.let { { Text(it) } },
                modifier = Modifier.fillMaxWidth(),
            )
        },
        confirmButton = {
            TextButton(onClick = { onConfirm(text) }, enabled = text.isNotBlank()) { Text(confirm) }
        },
        dismissButton = {
            TextButton(onClick = onDismiss) { Text(stringResource(R.string.dialog_cancel)) }
        },
    )
}

/** Download-and-hand-off: expose the cached file via FileProvider to a viewer/share sheet. */
private fun openFile(context: Context, file: File, share: Boolean): Boolean {
    val uri = FileProvider.getUriForFile(context, context.packageName + ".fileprovider", file)
    val ext = file.extension.lowercase()
    val mime = MimeTypeMap.getSingleton().getMimeTypeFromExtension(ext) ?: "*/*"
    val intent = if (share) {
        Intent(Intent.ACTION_SEND).apply {
            type = mime
            putExtra(Intent.EXTRA_STREAM, uri)
            clipData = ClipData.newRawUri(file.name, uri)
        }
    } else {
        Intent(Intent.ACTION_VIEW).apply { setDataAndType(uri, mime) }
    }
    intent.addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_ACTIVITY_NEW_TASK)
    return runCatching {
        context.startActivity(Intent.createChooser(intent, file.name).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
    }.isSuccess
}
