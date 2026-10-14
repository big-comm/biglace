package org.communitybig.biglace.feature.settings

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import android.content.ClipData
import androidx.compose.ui.platform.ClipEntry
import androidx.compose.ui.platform.LocalClipboard
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.viewmodel.compose.viewModel
import androidx.lifecycle.viewmodel.initializer
import androidx.lifecycle.viewmodel.viewModelFactory
import org.communitybig.biglace.AppContainer
import org.communitybig.biglace.R
import org.communitybig.biglace.core.mesh.MeshState
import org.communitybig.biglace.core.update.AppUpdater
import org.communitybig.biglace.core.update.UpdateState
import org.communitybig.biglace.ui.PasswordField
import androidx.compose.runtime.rememberCoroutineScope
import kotlinx.coroutines.launch

@Composable
fun SettingsScreen(container: AppContainer, modifier: Modifier = Modifier) {
    val vm: SettingsViewModel = viewModel(
        factory = viewModelFactory { initializer { SettingsViewModel(container) } },
    )
    val state by vm.state.collectAsStateWithLifecycle()

    // Saveable: unsaved edits survive rotation and swiping to another tab. The
    // auth key is a secret, so it deliberately stays out of saved state.
    var server by rememberSaveable { mutableStateOf(vm.initialServer()) }
    var authKey by remember { mutableStateOf(vm.initialAuthKey()) }
    var hostname by rememberSaveable { mutableStateOf(sanitizeDeviceName(vm.initialHostname())) }
    var autoConnect by remember { mutableStateOf(vm.autoConnect) }
    var bootConnect by remember { mutableStateOf(vm.connectOnBoot) }
    var showPanelLogin by remember { mutableStateOf(false) }
    var confirmForgetDevice by rememberSaveable { mutableStateOf(false) }
    val validHostname = hostname.isNotBlank()

    Column(
        modifier
            .fillMaxWidth()
            .verticalScroll(rememberScrollState())
            .padding(16.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        SectionTitle(stringResource(R.string.settings_connection))

        OutlinedTextField(
            value = server, onValueChange = { server = it },
            label = { Text(stringResource(R.string.settings_server_url)) },
            singleLine = true, modifier = Modifier.fillMaxWidth(),
        )
        // The key is only needed to enrol. Once the device has its own identity
        // an empty field is normal — say so instead of looking like lost data.
        val registered = remember(state) { vm.hasDeviceIdentity() }
        PasswordField(
            authKey, { authKey = it }, stringResource(R.string.settings_authkey),
            Modifier.fillMaxWidth(),
            supportingText = if (authKey.isBlank() && registered) {
                stringResource(R.string.settings_authkey_not_needed)
            } else null,
        )
        OutlinedTextField(
            value = hostname, onValueChange = { hostname = sanitizeDeviceNameWhileTyping(it) },
            label = { Text(stringResource(R.string.settings_device_name)) },
            isError = !validHostname,
            supportingText = if (!validHostname) {
                { Text(stringResource(R.string.settings_device_name_required)) }
            } else null,
            singleLine = true, modifier = Modifier.fillMaxWidth(),
        )

        Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
            Text(stringResource(R.string.settings_auto_connect), Modifier.weight(1f))
            Switch(checked = autoConnect, onCheckedChange = { autoConnect = it; vm.autoConnect = it })
        }
        Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
            Column(Modifier.weight(1f)) {
                Text(stringResource(R.string.settings_connect_on_boot))
                Text(
                    stringResource(R.string.settings_connect_on_boot_desc),
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurface.copy(alpha = 0.6f),
                )
            }
            Switch(checked = bootConnect, onCheckedChange = { bootConnect = it; vm.connectOnBoot = it })
        }
        Text(
            stringResource(R.string.settings_background_note),
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurface.copy(alpha = 0.6f),
        )

        Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            OutlinedButton(
                onClick = { vm.save(server, authKey, hostname) },
                enabled = validHostname,
                modifier = Modifier.weight(1f),
            ) { Text(stringResource(R.string.settings_save)) }

            val connected = state is MeshState.Up
            val connecting = state == MeshState.Connecting
            Button(
                onClick = {
                    if (connected) vm.disconnect()
                    else vm.connect(server, authKey, hostname)
                },
                modifier = Modifier.weight(1f),
                enabled = !connecting && (connected || (validHostname && server.isNotBlank() &&
                    (authKey.isNotBlank() || container.hasConnectConfig()))),
                colors = if (connected) {
                    ButtonDefaults.buttonColors(containerColor = MaterialTheme.colorScheme.error)
                } else ButtonDefaults.buttonColors(),
            ) {
                Text(
                    stringResource(
                        when {
                            connected -> R.string.settings_disconnect
                            connecting -> R.string.ssh_connecting
                            else -> R.string.settings_connect
                        },
                    ),
                )
            }
        }

        OutlinedButton(onClick = { showPanelLogin = true }, modifier = Modifier.fillMaxWidth()) {
            Text(stringResource(R.string.settings_panel_signin))
        }
        TextButton(onClick = { confirmForgetDevice = true }, modifier = Modifier.fillMaxWidth()) {
            Text(stringResource(R.string.settings_forget_device), color = MaterialTheme.colorScheme.error)
        }

        HorizontalDivider(Modifier.padding(vertical = 8.dp))
        SectionTitle(stringResource(R.string.ssh_section))
        SshKeySection(vm)

        HorizontalDivider(Modifier.padding(vertical = 8.dp))
        SectionTitle(stringResource(R.string.settings_updates))
        UpdatesSection(container.updater)

        HorizontalDivider(Modifier.padding(vertical = 8.dp))
        SectionTitle(stringResource(R.string.settings_about))
        Text(
            stringResource(R.string.about_status),
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurface.copy(alpha = 0.6f),
        )
    }

    if (confirmForgetDevice) {
        AlertDialog(
            onDismissRequest = { confirmForgetDevice = false },
            title = { Text(stringResource(R.string.settings_forget_device)) },
            text = { Text(stringResource(R.string.settings_forget_device_confirm)) },
            confirmButton = {
                TextButton(onClick = {
                    confirmForgetDevice = false
                    vm.forgetDevice { authKey = "" }
                }) {
                    Text(stringResource(R.string.action_forget), color = MaterialTheme.colorScheme.error)
                }
            },
            dismissButton = {
                TextButton(onClick = { confirmForgetDevice = false }) {
                    Text(stringResource(R.string.dialog_cancel))
                }
            },
        )
    }

    if (showPanelLogin) {
        PanelLoginDialog(
            vm = vm,
            onDismiss = { showPanelLogin = false },
            onSuccess = {
                server = vm.initialServer()
                authKey = vm.initialAuthKey()
                hostname = vm.initialHostname()
                showPanelLogin = false
            },
        )
    }
}

@Composable
private fun UpdatesSection(updater: AppUpdater) {
    val state by updater.state.collectAsStateWithLifecycle()
    val busy = state is UpdateState.Checking || state is UpdateState.Downloading
    Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
        Text(
            stringResource(R.string.update_installed_version, updater.installedVersionName),
            style = MaterialTheme.typography.bodyMedium,
        )
        val status = when (val s = state) {
            UpdateState.Checking -> stringResource(R.string.update_checking)
            is UpdateState.UpToDate -> stringResource(R.string.update_up_to_date, s.versionName)
            is UpdateState.Failed -> if (s.release == null) s.message else null
            else -> null
        }
        status?.let {
            Text(
                it,
                style = MaterialTheme.typography.bodySmall,
                color = if (state is UpdateState.Failed) MaterialTheme.colorScheme.error
                else MaterialTheme.colorScheme.onSurface.copy(alpha = 0.7f),
            )
        }
        OutlinedButton(
            onClick = { updater.check(manual = true) },
            enabled = !busy,
            modifier = Modifier.fillMaxWidth(),
        ) { Text(stringResource(R.string.update_check)) }
    }
}

@Composable
private fun SectionTitle(text: String) {
    Text(text, style = MaterialTheme.typography.titleMedium, color = MaterialTheme.colorScheme.primary)
}

@Composable
private fun SshKeySection(vm: SettingsViewModel) {
    val pub by vm.sshPublicKey.collectAsStateWithLifecycle()
    val generating by vm.generatingKey.collectAsStateWithLifecycle()
    val keyError by vm.keyError.collectAsStateWithLifecycle()
    val clipboard = LocalClipboard.current
    val scope = rememberCoroutineScope()
    val context = LocalContext.current
    val clipLabel = stringResource(R.string.ssh_share_title)
    var expanded by rememberSaveable { mutableStateOf(false) }
    var confirmClearHosts by rememberSaveable { mutableStateOf(false) }

    Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
        keyError?.let { Text(it, color = MaterialTheme.colorScheme.error) }
        if (pub.isBlank()) {
            Button(
                onClick = { vm.generateSshKey() },
                enabled = !generating,
                modifier = Modifier.fillMaxWidth(),
            ) { Text(stringResource(if (generating) R.string.ssh_generating else R.string.ssh_generate)) }
        } else {
            OutlinedButton(
                onClick = { expanded = !expanded },
                modifier = Modifier.fillMaxWidth(),
            ) {
                Text(stringResource(if (expanded) R.string.ssh_hide_key else R.string.ssh_show_key))
            }
            if (expanded) {
                Text(
                    stringResource(R.string.ssh_public_key),
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.onSurface.copy(alpha = 0.6f),
                )
                androidx.compose.foundation.text.selection.SelectionContainer {
                    Text(pub, fontFamily = FontFamily.Monospace, style = MaterialTheme.typography.bodySmall)
                }
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    Button(
                        onClick = {
                            scope.launch {
                                clipboard.setClipEntry(
                                    ClipEntry(ClipData.newPlainText(clipLabel, pub)),
                                )
                            }
                        },
                        modifier = Modifier.weight(1f),
                    ) {
                        Text(stringResource(R.string.action_copy))
                    }
                    OutlinedButton(onClick = { sharePublicKey(context, pub) }, modifier = Modifier.weight(1f)) {
                        Text(stringResource(R.string.action_share))
                    }
                }
            }
            TextButton(onClick = { vm.generateSshKey() }, enabled = !generating) {
                Text(stringResource(if (generating) R.string.ssh_generating else R.string.ssh_regenerate))
            }
            Text(
                stringResource(R.string.ssh_key_info),
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurface.copy(alpha = 0.6f),
            )
        }
        OutlinedButton(
            onClick = { confirmClearHosts = true },
            modifier = Modifier.fillMaxWidth(),
        ) {
            Text(stringResource(R.string.ssh_forget_trusted_hosts))
        }
    }

    if (confirmClearHosts) {
        AlertDialog(
            onDismissRequest = { confirmClearHosts = false },
            title = { Text(stringResource(R.string.ssh_forget_trusted_hosts)) },
            text = { Text(stringResource(R.string.ssh_forget_trusted_hosts_confirm)) },
            confirmButton = {
                TextButton(onClick = {
                    vm.clearTrustedSshHosts()
                    confirmClearHosts = false
                }) {
                    Text(stringResource(R.string.action_forget))
                }
            },
            dismissButton = {
                TextButton(onClick = { confirmClearHosts = false }) {
                    Text(stringResource(R.string.dialog_cancel))
                }
            },
        )
    }
}

private fun sharePublicKey(context: android.content.Context, pub: String) {
    val intent = android.content.Intent(android.content.Intent.ACTION_SEND).apply {
        type = "text/plain"
        putExtra(android.content.Intent.EXTRA_TEXT, pub)
    }
    context.startActivity(
        android.content.Intent.createChooser(intent, context.getString(R.string.ssh_share_title)),
    )
}
