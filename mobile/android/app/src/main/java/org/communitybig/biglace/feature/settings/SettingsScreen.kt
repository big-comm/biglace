package org.communitybig.biglace.feature.settings

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalClipboardManager
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.viewmodel.compose.viewModel
import androidx.lifecycle.viewmodel.initializer
import androidx.lifecycle.viewmodel.viewModelFactory
import org.communitybig.biglace.AppContainer
import org.communitybig.biglace.R
import org.communitybig.biglace.core.mesh.MeshState
import org.communitybig.biglace.ui.PasswordField

@Composable
fun SettingsScreen(container: AppContainer, modifier: Modifier = Modifier) {
    val vm: SettingsViewModel = viewModel(
        factory = viewModelFactory { initializer { SettingsViewModel(container) } },
    )
    val state by vm.state.collectAsStateWithLifecycle()

    var server by remember { mutableStateOf(vm.initialServer()) }
    var authKey by remember { mutableStateOf(vm.initialAuthKey()) }
    var hostname by remember { mutableStateOf(vm.initialHostname()) }
    var autoConnect by remember { mutableStateOf(vm.initialAutoConnect()) }
    var bootConnect by remember { mutableStateOf(vm.connectOnBoot) }
    var showPanelLogin by remember { mutableStateOf(false) }

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
        PasswordField(authKey, { authKey = it }, stringResource(R.string.settings_authkey),
            Modifier.fillMaxWidth())
        OutlinedTextField(
            value = hostname, onValueChange = { hostname = it },
            label = { Text(stringResource(R.string.settings_device_name)) },
            singleLine = true, modifier = Modifier.fillMaxWidth(),
        )

        Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
            Text(stringResource(R.string.settings_auto_connect), Modifier.weight(1f))
            Switch(checked = autoConnect, onCheckedChange = { autoConnect = it })
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
                onClick = { vm.save(server, authKey, hostname, autoConnect) },
                modifier = Modifier.weight(1f),
            ) { Text(stringResource(R.string.settings_save)) }

            val connected = state is MeshState.Up
            Button(
                onClick = {
                    if (connected) vm.disconnect()
                    else vm.connect(server, authKey, hostname)
                },
                modifier = Modifier.weight(1f),
                colors = if (connected) {
                    ButtonDefaults.buttonColors(containerColor = MaterialTheme.colorScheme.error)
                } else ButtonDefaults.buttonColors(),
            ) {
                Text(stringResource(if (connected) R.string.settings_disconnect else R.string.settings_connect))
            }
        }

        OutlinedButton(onClick = { showPanelLogin = true }, modifier = Modifier.fillMaxWidth()) {
            Text(stringResource(R.string.settings_panel_signin))
        }

        HorizontalDivider(Modifier.padding(vertical = 8.dp))
        SectionTitle(stringResource(R.string.ssh_section))
        SshKeySection(vm)

        HorizontalDivider(Modifier.padding(vertical = 8.dp))
        SectionTitle(stringResource(R.string.settings_about))
        Text(
            stringResource(R.string.about_status),
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurface.copy(alpha = 0.6f),
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
private fun SectionTitle(text: String) {
    Text(text, style = MaterialTheme.typography.titleMedium, color = MaterialTheme.colorScheme.primary)
}

@Composable
private fun SshKeySection(vm: SettingsViewModel) {
    val pub by vm.sshPublicKey.collectAsStateWithLifecycle()
    val generating by vm.generatingKey.collectAsStateWithLifecycle()
    val clipboard = LocalClipboardManager.current
    val context = LocalContext.current

    Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
        if (pub.isBlank()) {
            Button(
                onClick = { vm.generateSshKey() },
                enabled = !generating,
                modifier = Modifier.fillMaxWidth(),
            ) { Text(stringResource(if (generating) R.string.ssh_generating else R.string.ssh_generate)) }
        } else {
            Text(
                stringResource(R.string.ssh_public_key),
                style = MaterialTheme.typography.labelSmall,
                color = MaterialTheme.colorScheme.onSurface.copy(alpha = 0.6f),
            )
            androidx.compose.foundation.text.selection.SelectionContainer {
                Text(pub, fontFamily = FontFamily.Monospace, style = MaterialTheme.typography.bodySmall)
            }
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                Button(onClick = { clipboard.setText(AnnotatedString(pub)) }, modifier = Modifier.weight(1f)) {
                    Text(stringResource(R.string.action_copy))
                }
                OutlinedButton(onClick = { sharePublicKey(context, pub) }, modifier = Modifier.weight(1f)) {
                    Text(stringResource(R.string.action_share))
                }
            }
            androidx.compose.material3.TextButton(onClick = { vm.generateSshKey() }, enabled = !generating) {
                Text(stringResource(if (generating) R.string.ssh_generating else R.string.ssh_regenerate))
            }
            Text(
                stringResource(R.string.ssh_key_info),
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurface.copy(alpha = 0.6f),
            )
        }
    }
}

private fun sharePublicKey(context: android.content.Context, pub: String) {
    val intent = android.content.Intent(android.content.Intent.ACTION_SEND).apply {
        type = "text/plain"
        putExtra(android.content.Intent.EXTRA_TEXT, pub)
    }
    context.startActivity(android.content.Intent.createChooser(intent, "SSH public key"))
}
