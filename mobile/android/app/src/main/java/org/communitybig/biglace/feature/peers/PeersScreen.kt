package org.communitybig.biglace.feature.peers

import androidx.compose.animation.animateContentSize
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Check
import androidx.compose.material.icons.filled.Home
import androidx.compose.material.icons.filled.KeyboardArrowDown
import androidx.compose.material.icons.filled.KeyboardArrowUp
import androidx.compose.material.icons.filled.Refresh
import androidx.compose.material.icons.filled.Star
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ElevatedCard
import androidx.compose.material3.FilledTonalButton
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import android.widget.Toast
import androidx.compose.ui.platform.LocalClipboardManager
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.viewmodel.compose.viewModel
import androidx.lifecycle.viewmodel.initializer
import androidx.lifecycle.viewmodel.viewModelFactory
import org.communitybig.biglace.AppContainer
import org.communitybig.biglace.PendingTarget
import org.communitybig.biglace.R
import org.communitybig.biglace.core.mesh.MeshState
import org.communitybig.biglace.core.mesh.Peer
import org.communitybig.biglace.ui.theme.Offline
import org.communitybig.biglace.ui.theme.Online

@Composable
fun PeersScreen(
    container: AppContainer,
    onOpenTerminal: () -> Unit = {},
    onOpenFiles: () -> Unit = {},
    modifier: Modifier = Modifier,
) {
    val vm: PeersViewModel = viewModel(
        factory = viewModelFactory { initializer { PeersViewModel(container) } },
    )
    val ui by vm.ui.collectAsStateWithLifecycle()
    var expandedHosts by remember { mutableStateOf(setOf<String>()) }

    LazyColumn(
        modifier = modifier.fillMaxWidth(),
        contentPadding = PaddingValues(16.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        item { AppVersionLabel() }
        item { ConnectionCard(ui.state, vm) }

        val connected = ui.state is MeshState.Up
        if (connected && ui.items.isNotEmpty()) {
            items(ui.items, key = { it.peer.hostname }) { item ->
                val host = item.peer.hostname
                PeerCard(
                    item = item,
                    expanded = host in expandedHosts,
                    onToggleExpand = {
                        expandedHosts =
                            if (host in expandedHosts) expandedHosts - host else expandedHosts + host
                    },
                    onToggleFavorite = { vm.toggleFavorite(host) },
                    onSetOverride = { vm.setOverride(host, it) },
                    onTerminal = {
                        container.pendingTarget.value =
                            PendingTarget(targetHost(item.peer), resolveLogin(item), false)
                        onOpenTerminal()
                    },
                    onFiles = {
                        container.pendingTarget.value =
                            PendingTarget(targetHost(item.peer), resolveLogin(item), true)
                        onOpenFiles()
                    },
                )
            }
        } else if (connected) {
            item {
                Text(
                    stringResource(R.string.peers_empty),
                    color = MaterialTheme.colorScheme.onSurface.copy(alpha = 0.6f),
                    modifier = Modifier.padding(top = 24.dp),
                    textAlign = TextAlign.Center,
                )
            }
        }
    }
}

@Composable
private fun AppVersionLabel() {
    val context = LocalContext.current
    val version = remember {
        runCatching {
            context.packageManager.getPackageInfo(context.packageName, 0).versionName
        }.getOrNull().orEmpty()
    }
    if (version.isNotBlank()) {
        Text(
            "v$version",
            style = MaterialTheme.typography.labelSmall,
            color = MaterialTheme.colorScheme.onSurface.copy(alpha = 0.4f),
            textAlign = TextAlign.End,
            modifier = Modifier.fillMaxWidth().padding(end = 4.dp),
        )
    }
}

@Composable
private fun ConnectionCard(state: MeshState, vm: PeersViewModel) {
    val (dot, title) = when (state) {
        is MeshState.Up -> Online to stringResource(R.string.peers_connected)
        MeshState.Connecting -> MaterialTheme.colorScheme.secondary to stringResource(R.string.peers_connecting)
        is MeshState.Error -> MaterialTheme.colorScheme.error to stringResource(R.string.peers_error_title)
        MeshState.Disconnected -> Offline to stringResource(R.string.peers_disconnected)
    }
    ElevatedCard(Modifier.fillMaxWidth()) {
        Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(12.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                if (state == MeshState.Connecting) {
                    CircularProgressIndicator(Modifier.size(16.dp), strokeWidth = 2.dp)
                } else {
                    Box(Modifier.size(12.dp).clip(CircleShape).background(dot))
                }
                Text(
                    title,
                    style = MaterialTheme.typography.titleMedium,
                    fontWeight = FontWeight.SemiBold,
                    modifier = Modifier.padding(start = 12.dp).weight(1f),
                )
                if (state is MeshState.Up) {
                    IconButton(onClick = { vm.refresh() }) {
                        Icon(Icons.Filled.Refresh, contentDescription = stringResource(R.string.action_refresh))
                    }
                }
            }
            when (state) {
                MeshState.Connecting -> Text(
                    stringResource(R.string.peers_connecting_hint),
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurface.copy(alpha = 0.6f),
                )
                is MeshState.Error -> {
                    androidx.compose.foundation.text.selection.SelectionContainer {
                        Text(
                            state.message,
                            style = MaterialTheme.typography.bodySmall,
                            fontFamily = FontFamily.Monospace,
                            color = MaterialTheme.colorScheme.error,
                        )
                    }
                    Button(onClick = { vm.connect() }, enabled = vm.canConnect()) {
                        Text(stringResource(R.string.peers_retry))
                    }
                }
                is MeshState.Up -> FilledTonalButton(onClick = { vm.disconnect() }) {
                    Text(stringResource(R.string.settings_disconnect))
                }
                MeshState.Disconnected -> {
                    if (vm.canConnect()) {
                        Button(onClick = { vm.connect() }, modifier = Modifier.fillMaxWidth()) {
                            Text(stringResource(R.string.settings_connect))
                        }
                    } else {
                        Text(
                            stringResource(R.string.peers_configure),
                            style = MaterialTheme.typography.bodySmall,
                            color = MaterialTheme.colorScheme.onSurface.copy(alpha = 0.6f),
                        )
                    }
                }
            }
        }
    }
}

@Composable
private fun PeerCard(
    item: PeerItem,
    expanded: Boolean,
    onToggleExpand: () -> Unit,
    onToggleFavorite: () -> Unit,
    onSetOverride: (String) -> Unit,
    onTerminal: () -> Unit,
    onFiles: () -> Unit,
) {
    val peer = item.peer
    Card(
        Modifier.fillMaxWidth().animateContentSize(),
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surface),
    ) {
        Column {
            // ── Header (tap to expand) ──
            Row(
                Modifier.fillMaxWidth().clickable { onToggleExpand() }
                    .padding(start = 14.dp, top = 12.dp, bottom = 12.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Box(Modifier.size(10.dp).clip(CircleShape).background(if (peer.online) Online else Offline))
                Column(Modifier.padding(start = 12.dp).weight(1f)) {
                    Text(peer.displayName, style = MaterialTheme.typography.titleSmall, fontWeight = FontWeight.SemiBold)
                    Text(
                        peerSubtitle(peer),
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurface.copy(alpha = 0.6f),
                    )
                }
                if (peer.online) {
                    IconButton(onClick = onTerminal) {
                        Icon(painterResource(R.drawable.ic_terminal), contentDescription = stringResource(R.string.action_terminal),
                            tint = MaterialTheme.colorScheme.primary)
                    }
                    IconButton(onClick = onFiles) {
                        Icon(painterResource(R.drawable.ic_folder), contentDescription = stringResource(R.string.action_files),
                            tint = MaterialTheme.colorScheme.primary)
                    }
                }
                IconButton(onClick = onToggleFavorite) {
                    Icon(
                        Icons.Filled.Star,
                        contentDescription = stringResource(if (item.isFavorite) R.string.action_unpin else R.string.action_pin),
                        tint = if (item.isFavorite) MaterialTheme.colorScheme.primary
                        else MaterialTheme.colorScheme.onSurface.copy(alpha = 0.3f),
                    )
                }
                Icon(
                    if (expanded) Icons.Filled.KeyboardArrowUp else Icons.Filled.KeyboardArrowDown,
                    contentDescription = null,
                    modifier = Modifier.padding(end = 8.dp),
                    tint = MaterialTheme.colorScheme.onSurface.copy(alpha = 0.5f),
                )
            }

            // ── Details (when expanded) ──
            if (expanded) {
                HorizontalDivider(color = MaterialTheme.colorScheme.onSurface.copy(alpha = 0.08f))
                if (peer.dnsName.isNotBlank()) DetailRow(stringResource(R.string.peer_dns), peer.dnsName, copyable = true)
                peer.ipv4?.let { DetailRow("IPv4", it, copyable = true) }
                peer.ipv6?.let { DetailRow("IPv6", it, copyable = true) }
                if (peer.owner.isNotBlank()) DetailRow(stringResource(R.string.peer_owner), peer.owner)
                if (peer.tags.isNotEmpty()) DetailRow(stringResource(R.string.peer_tags), peer.tags.joinToString(", "))
                if (!peer.online && !peer.lastSeen.isNullOrBlank()) {
                    DetailRow(stringResource(R.string.peer_last_seen), peer.lastSeen!!)
                }
                OverrideRow(item, onSetOverride)
            }
        }
    }
}

@Composable
private fun DetailRow(label: String, value: String, copyable: Boolean = false) {
    val clipboard = LocalClipboardManager.current
    Row(
        Modifier.fillMaxWidth().padding(start = 14.dp, end = 4.dp, top = 8.dp, bottom = 8.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Column(Modifier.weight(1f)) {
            Text(label, style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.onSurface.copy(alpha = 0.6f))
            Text(value, style = MaterialTheme.typography.bodyMedium, fontFamily = FontFamily.Monospace)
        }
        if (copyable) {
            IconButton(onClick = { clipboard.setText(AnnotatedString(value)) }) {
                Icon(painterResource(R.drawable.ic_copy), contentDescription = stringResource(R.string.action_copy),
                    tint = MaterialTheme.colorScheme.onSurface.copy(alpha = 0.6f))
            }
        }
    }
}

@Composable
private fun OverrideRow(item: PeerItem, onSetOverride: (String) -> Unit) {
    var text by remember(item.override) { mutableStateOf(item.override ?: "") }
    val auto = autoLogin(item.peer)
    val context = LocalContext.current
    val savedMsg = stringResource(R.string.peer_login_saved)
    val clearedMsg = stringResource(R.string.peer_login_cleared, auto)
    Row(
        Modifier.fillMaxWidth().padding(start = 14.dp, end = 8.dp, top = 4.dp, bottom = 12.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        OutlinedTextField(
            value = text,
            onValueChange = { text = it },
            label = { Text(stringResource(R.string.peer_ssh_override)) },
            placeholder = { Text(stringResource(R.string.peer_ssh_override_hint, auto)) },
            singleLine = true,
            modifier = Modifier.weight(1f),
        )
        IconButton(onClick = {
            val value = text.trim()
            text = value
            onSetOverride(value)
            val msg = if (value.isBlank()) clearedMsg else "$savedMsg $value"
            Toast.makeText(context, msg, Toast.LENGTH_SHORT).show()
        }) {
            Icon(Icons.Filled.Check, contentDescription = stringResource(R.string.action_save),
                tint = MaterialTheme.colorScheme.primary)
        }
    }
}

private fun peerSubtitle(peer: Peer): String = buildList {
    peer.ip?.let { add(it) }
    if (peer.os.isNotEmpty()) add(peer.os.replaceFirstChar { it.uppercase() })
    when {
        !peer.online -> add("offline")
        peer.latencyMs != null -> add("${peer.latencyMs} ms")
    }
}.joinToString("  ·  ")

/** SSH target host: prefer the tailnet IP (tsnet dials it directly), else DNS. */
private fun targetHost(peer: Peer): String = peer.ip ?: peer.dnsName

/** Auto-resolved login (before user override): panel os_user → owner → hostname. */
private fun autoLogin(peer: Peer): String =
    peer.sshUser?.takeIf { it.isNotBlank() }
        ?: peer.owner.takeIf { it.isNotBlank() }
        ?: peer.hostname

/** Login actually used: override → auto. */
private fun resolveLogin(item: PeerItem): String =
    item.override?.takeIf { it.isNotBlank() } ?: autoLogin(item.peer)
