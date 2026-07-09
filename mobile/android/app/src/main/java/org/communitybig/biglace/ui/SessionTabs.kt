package org.communitybig.biglace.ui

import androidx.compose.foundation.clickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Add
import androidx.compose.material.icons.filled.Close
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import org.communitybig.biglace.R
import org.communitybig.biglace.core.ssh.SshSessionTab

@Composable
fun SessionTabs(
    tabs: List<SshSessionTab>,
    activeId: Long,
    onSelect: (Long) -> Unit,
    onAdd: () -> Unit,
    onCloseActive: () -> Unit,
    modifier: Modifier = Modifier,
) {
    Surface(modifier = modifier, color = MaterialTheme.colorScheme.surfaceVariant) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Row(
                modifier = Modifier.weight(1f).horizontalScroll(rememberScrollState())
                    .padding(horizontal = 6.dp, vertical = 5.dp),
                horizontalArrangement = Arrangement.spacedBy(6.dp),
            ) {
                tabs.forEach { tab ->
                    val selected = tab.id == activeId
                    Surface(
                        shape = RoundedCornerShape(18.dp),
                        color = if (selected) MaterialTheme.colorScheme.primaryContainer
                        else MaterialTheme.colorScheme.surface,
                    ) {
                        Row(
                            modifier = Modifier.clickable { onSelect(tab.id) }
                                .padding(start = 12.dp, end = if (selected) 2.dp else 12.dp),
                            verticalAlignment = Alignment.CenterVertically,
                        ) {
                            Text(
                                tab.host.ifBlank { stringResource(R.string.session_new) },
                                maxLines = 1,
                                overflow = TextOverflow.Ellipsis,
                                style = MaterialTheme.typography.labelLarge,
                            )
                            if (selected) {
                                IconButton(onClick = onCloseActive, modifier = Modifier.size(32.dp)) {
                                    Icon(
                                        Icons.Filled.Close,
                                        contentDescription = stringResource(R.string.session_close),
                                        modifier = Modifier.size(17.dp),
                                    )
                                }
                            }
                        }
                    }
                }
            }
            IconButton(onClick = onAdd) {
                Icon(Icons.Filled.Add, contentDescription = stringResource(R.string.session_add))
            }
        }
    }
}
