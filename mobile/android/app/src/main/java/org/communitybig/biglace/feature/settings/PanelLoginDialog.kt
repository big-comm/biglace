package org.communitybig.biglace.feature.settings

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.launch
import org.communitybig.biglace.R
import org.communitybig.biglace.ui.PasswordField

/**
 * Panel sign-in dialog: mirrors the desktop "Sign in with panel account" flow.
 * Fetches a fresh pre-auth key via [SettingsViewModel.panelSignIn] and, on
 * success, fills the connection form. The Sign-in button is disabled while the
 * request is in flight so it can't be fired twice.
 */
@Composable
fun PanelLoginDialog(
    vm: SettingsViewModel,
    onDismiss: () -> Unit,
    onSuccess: () -> Unit,
) {
    val scope = rememberCoroutineScope()
    val fillFieldsMsg = stringResource(R.string.panel_fill_fields)
    val signInFailedMsg = stringResource(R.string.panel_signin_failed)

    var url by remember { mutableStateOf(vm.initialPanelUrl()) }
    var user by remember { mutableStateOf(vm.initialPanelUsername()) }
    var password by remember { mutableStateOf("") }
    var node by remember { mutableStateOf(sanitizeDeviceName(vm.initialHostname())) }
    var busy by remember { mutableStateOf(false) }
    var error by remember { mutableStateOf<String?>(null) }

    AlertDialog(
        onDismissRequest = { if (!busy) onDismiss() },
        title = { Text(stringResource(R.string.settings_panel_signin)) },
        text = {
            Column(Modifier.fillMaxWidth(), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                OutlinedTextField(url, { url = it }, enabled = !busy, singleLine = true,
                    label = { Text(stringResource(R.string.settings_panel_url)) })
                OutlinedTextField(user, { user = it }, enabled = !busy, singleLine = true,
                    label = { Text(stringResource(R.string.settings_username)) })
                PasswordField(password, { password = it }, stringResource(R.string.settings_password),
                    modifier = Modifier.fillMaxWidth(), enabled = !busy)
                OutlinedTextField(node, { node = sanitizeDeviceName(it) }, enabled = !busy, singleLine = true,
                    label = { Text(stringResource(R.string.settings_device_name)) })
                error?.let { Text(it, color = MaterialTheme.colorScheme.error) }
                if (busy) {
                    CircularProgressIndicator(Modifier.padding(top = 4.dp))
                    Text(stringResource(R.string.panel_signing_in))
                }
            }
        },
        confirmButton = {
            TextButton(
                enabled = !busy,
                onClick = {
                    if (url.isBlank() || user.isBlank() || password.isBlank() || node.isBlank()) {
                        error = fillFieldsMsg
                        return@TextButton
                    }
                    busy = true
                    error = null
                    scope.launch {
                        vm.panelSignIn(url, user, password, node)
                            .onSuccess { busy = false; onSuccess() }
                            .onFailure { busy = false; error = it.message ?: signInFailedMsg }
                    }
                },
            ) { Text(stringResource(R.string.dialog_signin)) }
        },
        dismissButton = {
            TextButton(enabled = !busy, onClick = onDismiss) { Text(stringResource(R.string.dialog_cancel)) }
        },
    )
}
