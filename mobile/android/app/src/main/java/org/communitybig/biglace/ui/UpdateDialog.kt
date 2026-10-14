package org.communitybig.biglace.ui

import android.text.format.Formatter
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.unit.dp
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.compose.LifecycleEventEffect
import org.communitybig.biglace.R
import org.communitybig.biglace.core.update.AppRelease
import org.communitybig.biglace.core.update.AppUpdater
import org.communitybig.biglace.core.update.UpdateState

/**
 * Offers a newer app version found on GitHub: release notes, download with
 * progress, then the Android installer. Shown for states that carry a release;
 * plain status ("up to date", check failed) is shown in Settings instead.
 */
@Composable
fun UpdateDialog(updater: AppUpdater, state: UpdateState) {
    val release: AppRelease = when (state) {
        is UpdateState.Available -> state.release
        is UpdateState.Downloading -> state.release
        is UpdateState.ReadyToInstall -> state.release
        is UpdateState.Failed -> state.release ?: return
        else -> return
    }
    val context = LocalContext.current
    // Re-read on resume: the user grants "install unknown apps" in system
    // settings and comes back; a value read once would leave them stuck.
    var canInstall by remember { mutableStateOf(updater.canInstall()) }
    LifecycleEventEffect(Lifecycle.Event.ON_RESUME) { canInstall = updater.canInstall() }
    var installFailed by remember { mutableStateOf(false) }
    val downloading = state is UpdateState.Downloading

    AlertDialog(
        onDismissRequest = { if (!downloading) updater.postpone() },
        title = { Text(stringResource(R.string.update_available_title)) },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
                Text(
                    if (release.sizeBytes > 0) {
                        stringResource(
                            R.string.update_version_size,
                            release.versionName,
                            Formatter.formatShortFileSize(context, release.sizeBytes),
                        )
                    } else {
                        release.versionName
                    },
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurface.copy(alpha = 0.7f),
                )
                if (release.notes.isNotBlank()) {
                    // Long notes scroll inside their own box, so they can't push
                    // the buttons off screen.
                    Text(
                        release.notes,
                        style = MaterialTheme.typography.bodySmall,
                        modifier = Modifier
                            .fillMaxWidth()
                            .heightIn(max = 220.dp)
                            .verticalScroll(rememberScrollState()),
                    )
                }
                when {
                    state is UpdateState.Downloading -> {
                        if (state.progress >= 0f) {
                            LinearProgressIndicator(progress = { state.progress }, modifier = Modifier.fillMaxWidth())
                            Text(stringResource(R.string.update_downloading, (state.progress * 100).toInt()))
                        } else {
                            LinearProgressIndicator(modifier = Modifier.fillMaxWidth())
                            Text(stringResource(R.string.update_downloading_unknown))
                        }
                    }
                    state is UpdateState.Failed -> Text(state.message, color = MaterialTheme.colorScheme.error)
                    !canInstall -> Text(
                        stringResource(R.string.update_permission_needed),
                        style = MaterialTheme.typography.bodySmall,
                    )
                }
                if (installFailed) {
                    Text(stringResource(R.string.update_install_failed), color = MaterialTheme.colorScheme.error)
                }
            }
        },
        confirmButton = {
            when {
                downloading -> Unit
                !canInstall -> TextButton(onClick = updater::openInstallPermission) {
                    Text(stringResource(R.string.update_grant_permission))
                }
                state is UpdateState.ReadyToInstall -> TextButton(onClick = {
                    installFailed = !updater.install(state.apk)
                }) { Text(stringResource(R.string.update_install)) }
                state is UpdateState.Failed -> TextButton(onClick = { updater.download(release) }) {
                    Text(stringResource(R.string.update_retry))
                }
                else -> TextButton(onClick = { updater.download(release) }) {
                    Text(stringResource(R.string.update_now))
                }
            }
        },
        dismissButton = {
            if (!downloading) {
                TextButton(onClick = updater::postpone) { Text(stringResource(R.string.update_later)) }
            }
        },
    )
}
