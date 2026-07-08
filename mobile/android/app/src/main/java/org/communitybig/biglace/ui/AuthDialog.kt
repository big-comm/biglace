package org.communitybig.biglace.ui

import android.os.Build
import android.view.WindowManager
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.MaterialTheme
import androidx.compose.foundation.layout.Box
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.SideEffect
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.platform.LocalView
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.window.Dialog
import androidx.compose.ui.window.DialogProperties
import androidx.compose.ui.window.DialogWindowProvider

/**
 * A centred result/error dialog with a blurred backdrop (Android 12+; a dim
 * scrim elsewhere). Used for SSH auth results so a wrong user/password or a
 * rejected key is shown clearly enough to fix — instead of silently falling back.
 */
@Composable
fun AuthDialog(
    ok: Boolean,
    title: String,
    message: String,
    confirmText: String,
    onDismiss: () -> Unit,
) {
    Dialog(onDismissRequest = onDismiss, properties = DialogProperties(usePlatformDefaultWidth = false)) {
        val view = LocalView.current
        SideEffect {
            (view.parent as? DialogWindowProvider)?.window?.let { w ->
                w.setDimAmount(0.6f)
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
                    w.addFlags(WindowManager.LayoutParams.FLAG_BLUR_BEHIND)
                    w.attributes = w.attributes.apply { blurBehindRadius = 48 }
                }
            }
        }
        Surface(
            modifier = Modifier.fillMaxWidth(0.9f),
            shape = MaterialTheme.shapes.extraLarge,
            tonalElevation = 6.dp,
            shadowElevation = 12.dp,
        ) {
            Column(Modifier.padding(24.dp), verticalArrangement = Arrangement.spacedBy(14.dp)) {
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Box(
                        Modifier
                            .size(14.dp)
                            .clip(CircleShape)
                            .background(if (ok) MaterialTheme.colorScheme.primary else MaterialTheme.colorScheme.error),
                    )
                    Text(
                        title,
                        style = MaterialTheme.typography.titleLarge,
                        fontWeight = FontWeight.SemiBold,
                        modifier = Modifier.padding(start = 12.dp),
                    )
                }
                SelectionContainer {
                    Text(
                        message,
                        style = MaterialTheme.typography.bodyMedium,
                        fontFamily = FontFamily.Monospace,
                        color = MaterialTheme.colorScheme.onSurface.copy(alpha = 0.85f),
                        modifier = Modifier
                            .fillMaxWidth()
                            .heightIn(max = 340.dp)
                            .verticalScroll(rememberScrollState()),
                    )
                }
                TextButton(onClick = onDismiss, modifier = Modifier.align(Alignment.End)) {
                    Text(confirmText)
                }
            }
        }
    }
}
