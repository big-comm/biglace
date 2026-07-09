package org.communitybig.biglace.feature.terminal

import androidx.compose.foundation.background
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ElevatedCard
import androidx.compose.material3.FilterChip
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.saveable.rememberSaveableStateHolder
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.focus.onFocusChanged
import androidx.compose.ui.layout.onSizeChanged
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.font.Font
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardCapitalization
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.platform.LocalDensity
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.viewmodel.compose.viewModel
import androidx.lifecycle.viewmodel.initializer
import androidx.lifecycle.viewmodel.viewModelFactory
import org.communitybig.biglace.AppContainer
import org.communitybig.biglace.R
import org.communitybig.biglace.core.ssh.AuthMode
import org.communitybig.biglace.core.ssh.SshSessionTab
import org.communitybig.biglace.core.ssh.SshSessionTabsViewModel
import org.communitybig.biglace.ui.AuthDialog
import org.communitybig.biglace.ui.PasswordField
import org.communitybig.biglace.ui.SessionTabs
import kotlinx.coroutines.launch

@Composable
fun TerminalScreen(container: AppContainer, modifier: Modifier = Modifier) {
    val tabsVm: SshSessionTabsViewModel = viewModel(key = "terminal-tabs")
    val tabs by tabsVm.tabs.collectAsStateWithLifecycle()
    val activeId by tabsVm.activeId.collectAsStateWithLifecycle()
    val pending by container.pendingTarget.collectAsStateWithLifecycle()
    val stateHolder = rememberSaveableStateHolder()

    LaunchedEffect(pending) {
        pending?.takeIf { !it.wantFiles }?.let { target ->
            tabsVm.openPeer(
                target.host,
                container.secrets.sshUser(target.host) ?: target.user,
                container.secrets.sshPassword(target.host).orEmpty(),
            )
            container.pendingTarget.value = null
        }
    }

    val active = tabs.firstOrNull { it.id == activeId } ?: return
    val vm: TerminalViewModel = viewModel(
        key = "terminal-session-${active.id}",
        factory = viewModelFactory { initializer { TerminalViewModel(container) } },
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
            TerminalSessionScreen(
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
private fun TerminalSessionScreen(
    container: AppContainer,
    tab: SshSessionTab,
    vm: TerminalViewModel,
    onUpdate: ((SshSessionTab) -> SshSessionTab) -> Unit,
    modifier: Modifier = Modifier,
) {
    val status by vm.status.collectAsStateWithLifecycle()
    val screen by vm.screen.collectAsStateWithLifecycle()
    val testing by vm.testing.collectAsStateWithLifecycle()
    val probe by vm.probe.collectAsStateWithLifecycle()

    // Prefill the saved password whenever the host is known and none is typed yet.
    LaunchedEffect(tab.host) {
        if (tab.host.isNotBlank() && tab.password.isBlank()) {
            container.secrets.sshPassword(tab.host)?.let { saved ->
                onUpdate { it.copy(password = saved) }
            }
        }
    }

    if (status == TermStatus.Connected) {
        TerminalView(
            screen = screen,
            onRaw = vm::sendRaw,
            onCtrl = vm::sendCtrl,
            onKey = { vm.sendBytes(it) },
            onResize = vm::resize,
            onDisconnect = { vm.disconnect() },
            modifier = modifier,
        )
    } else {
        ConnectForm(
            iconRes = R.drawable.ic_terminal,
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
            connecting = status == TermStatus.Connecting,
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

    (status as? TermStatus.Error)?.let { err ->
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
}

@Composable
internal fun ConnectForm(
    iconRes: Int,
    host: String, onHost: (String) -> Unit,
    port: String, onPort: (String) -> Unit,
    user: String, onUser: (String) -> Unit,
    password: String, onPassword: (String) -> Unit,
    fromPeer: Boolean,
    connecting: Boolean,
    testing: Boolean,
    mode: AuthMode, onMode: (AuthMode) -> Unit,
    onConnect: () -> Unit,
    onTest: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val busy = connecting || testing
    Column(
        modifier.fillMaxSize().padding(16.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        ElevatedCard(Modifier.fillMaxWidth()) {
            Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(12.dp)) {
                if (fromPeer && host.isNotBlank()) {
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        Icon(painterResource(iconRes), contentDescription = null, tint = MaterialTheme.colorScheme.primary)
                        Text(
                            "$user@$host",
                            style = MaterialTheme.typography.titleMedium,
                            fontWeight = FontWeight.SemiBold,
                            fontFamily = FontFamily.Monospace,
                            modifier = Modifier.padding(start = 12.dp),
                        )
                    }
                    // Even from a peer, the login can be wrong — let it be edited.
                    OutlinedTextField(user, onUser, singleLine = true,
                        label = { Text(stringResource(R.string.settings_username)) }, modifier = Modifier.fillMaxWidth())
                } else {
                    OutlinedTextField(host, onHost, singleLine = true,
                        label = { Text(stringResource(R.string.ssh_host)) }, modifier = Modifier.fillMaxWidth())
                    Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                        OutlinedTextField(port, onPort, singleLine = true,
                            label = { Text(stringResource(R.string.ssh_port)) }, modifier = Modifier.width(96.dp))
                        OutlinedTextField(user, onUser, singleLine = true,
                            label = { Text(stringResource(R.string.settings_username)) }, modifier = Modifier.weight(1f))
                    }
                }
                Text(
                    stringResource(R.string.ssh_auth_mode),
                    style = MaterialTheme.typography.labelMedium,
                    color = MaterialTheme.colorScheme.onSurface.copy(alpha = 0.7f),
                )
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    AuthChip(stringResource(R.string.ssh_mode_auto), mode == AuthMode.AUTO) { onMode(AuthMode.AUTO) }
                    AuthChip(stringResource(R.string.ssh_mode_key), mode == AuthMode.KEY) { onMode(AuthMode.KEY) }
                    AuthChip(stringResource(R.string.ssh_mode_password), mode == AuthMode.PASSWORD) { onMode(AuthMode.PASSWORD) }
                }

                // A password only matters when we'll actually try it — hide it in
                // key-only mode, where all you need is the username.
                if (mode != AuthMode.KEY) {
                    PasswordField(password, onPassword, stringResource(R.string.settings_password),
                        Modifier.fillMaxWidth())
                }

                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    Button(
                        onClick = onConnect,
                        enabled = !busy && host.isNotBlank(),
                        modifier = Modifier.weight(1f),
                    ) {
                        if (connecting) {
                            CircularProgressIndicator(Modifier.size(16.dp), strokeWidth = 2.dp,
                                color = MaterialTheme.colorScheme.onPrimary)
                            Text(stringResource(R.string.ssh_connecting), Modifier.padding(start = 8.dp))
                        } else {
                            Text(stringResource(R.string.settings_connect))
                        }
                    }
                    OutlinedButton(onClick = onTest, enabled = !busy && host.isNotBlank()) {
                        if (testing) {
                            CircularProgressIndicator(Modifier.size(16.dp), strokeWidth = 2.dp)
                        } else {
                            Text(stringResource(R.string.ssh_test))
                        }
                    }
                }
            }
        }
        if (!fromPeer) {
            Text(
                stringResource(R.string.ssh_pick_peer),
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurface.copy(alpha = 0.6f),
            )
        }
    }
}

@Composable
private fun AuthChip(label: String, selected: Boolean, onClick: () -> Unit) {
    FilterChip(selected = selected, onClick = onClick, label = { Text(label) })
}

@Composable
private fun TerminalView(
    screen: AnnotatedString,
    onRaw: (String) -> Unit,
    onCtrl: (String) -> Unit,
    onKey: (Int) -> Unit,
    onResize: (Int, Int) -> Unit,
    onDisconnect: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val vScroll = rememberScrollState()
    val hScroll = rememberScrollState()
    val scope = rememberCoroutineScope()
    var fontSize by rememberSaveable { mutableFloatStateOf(13f) }
    var ctrl by rememberSaveable { mutableStateOf(false) }
    var buffer by rememberSaveable { mutableStateOf("") }
    var viewportWidth by remember { mutableIntStateOf(0) }
    var viewportHeight by remember { mutableIntStateOf(0) }
    val density = LocalDensity.current

    LaunchedEffect(viewportWidth, viewportHeight, fontSize, density) {
        if (viewportWidth > 0 && viewportHeight > 0) {
            val charPx = with(density) { fontSize.sp.toPx() } * 0.61f
            val linePx = with(density) { (fontSize * 1.25f).sp.toPx() }
            onResize(
                (viewportHeight / linePx).toInt().coerceAtLeast(2),
                (viewportWidth / charPx).toInt().coerceAtLeast(8),
            )
        }
    }

    // Keep the newest line (the prompt) in view: when little output, it sits at
    // the top; when it overflows, we follow the bottom.
    LaunchedEffect(screen) { vScroll.scrollTo(vScroll.maxValue) }

    Column(
        modifier.fillMaxSize().padding(horizontal = 8.dp, vertical = 4.dp),
        verticalArrangement = Arrangement.spacedBy(6.dp),
    ) {
        // ── Slim top bar: disconnect lives here, out of the input's way ──
        Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.End, verticalAlignment = Alignment.CenterVertically) {
            TextButton(onClick = onDisconnect, contentPadding = PaddingValues(horizontal = 10.dp, vertical = 2.dp)) {
                Text("✕  " + stringResource(R.string.settings_disconnect), style = MaterialTheme.typography.labelMedium)
            }
        }

        // ── Screen: top-aligned normal flow. Scrolls both ways for long/wide
        // output; pinch to zoom. ──
        Box(
            Modifier
                .weight(1f)
                .fillMaxWidth()
                .clip(RoundedCornerShape(10.dp))
                .background(TerminalBg)
                .onSizeChanged {
                    viewportWidth = it.width
                    viewportHeight = it.height
                }
                .pinchZoom { factor -> fontSize = (fontSize * factor).coerceIn(7f, 30f) }
                .verticalScroll(vScroll)
                .horizontalScroll(hScroll)
                .padding(10.dp),
        ) {
            Text(
                screen,
                fontFamily = TermFont,
                fontSize = fontSize.sp,
                lineHeight = (fontSize * 1.25f).sp,
                color = TerminalFg,
                softWrap = false,
            )
        }

        // ── Extra keys (hard-to-type keys the soft keyboard lacks) ──
        ExtraKeysRow(
            ctrl = ctrl,
            onCtrlToggle = { ctrl = !ctrl },
            onRaw = onRaw,
            onKey = onKey,
            onKill = { onRaw("\u0015clear\r"); buffer = "" }, // Ctrl-U (kill line) then run `clear`
        )

        // ── Input line + dedicated Enter button. Local buffer mirrors every edit
        // to the shell char-by-char, so echo, Tab and Ctrl behave like a real
        // terminal; Enter (button or key) sends CR and clears the buffer. ──
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp), verticalAlignment = Alignment.CenterVertically) {
            OutlinedTextField(
                value = buffer,
                onValueChange = { new ->
                    if (ctrl) {
                        val common = buffer.commonPrefixWith(new).length
                        if (new.length > common) { onCtrl(new.substring(common).take(1)); ctrl = false }
                        // don't keep the ctrl'd char in the buffer
                    } else {
                        val common = buffer.commonPrefixWith(new).length
                        repeat(buffer.length - common) { onKey(0x7F) }
                        if (new.length > common) onRaw(new.substring(common).replace("\n", "\r"))
                        buffer = new
                    }
                },
                singleLine = true,
                modifier = Modifier
                    .weight(1f)
                    .onFocusChanged { if (it.isFocused) scope.launch { vScroll.scrollTo(vScroll.maxValue) } },
                textStyle = MaterialTheme.typography.bodyLarge.copy(fontFamily = TermFont),
                keyboardOptions = KeyboardOptions(
                    capitalization = KeyboardCapitalization.None,
                    autoCorrectEnabled = false,
                    keyboardType = KeyboardType.Ascii,
                    imeAction = ImeAction.Go,
                ),
                keyboardActions = KeyboardActions(onGo = { onRaw("\r"); buffer = "" }),
                label = { Text(stringResource(R.string.ssh_type_hint)) },
            )
            Button(
                onClick = { onRaw("\r"); buffer = "" },
                contentPadding = PaddingValues(horizontal = 18.dp),
                modifier = Modifier.height(56.dp),
            ) {
                Text("⏎", style = MaterialTheme.typography.titleLarge)
            }
        }
    }
}

@Composable
private fun ExtraKeysRow(
    ctrl: Boolean,
    onCtrlToggle: () -> Unit,
    onRaw: (String) -> Unit,
    onKey: (Int) -> Unit,
    onKill: () -> Unit,
) {
    val scroll = rememberScrollState()
    Row(
        Modifier.fillMaxWidth().horizontalScroll(scroll),
        horizontalArrangement = Arrangement.spacedBy(6.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        KeyCap("ESC") { onKey(0x1B) }
        KeyCap("TAB") { onRaw("\t") }
        KeyCap("CTRL", active = ctrl, onClick = onCtrlToggle)
        KeyCap("⌫") { onKey(0x7F) }
        KeyCap("CLR", onClick = onKill)
        KeyCap("←") { onRaw("\u001B[D") }
        KeyCap("↑") { onRaw("\u001B[A") }
        KeyCap("↓") { onRaw("\u001B[B") }
        KeyCap("→") { onRaw("\u001B[C") }
        KeyCap("|") { onRaw("|") }
        KeyCap("/") { onRaw("/") }
        KeyCap("~") { onRaw("~") }
        KeyCap("-") { onRaw("-") }
        KeyCap("^C") { onKey(0x03) }
    }
}

@Composable
private fun KeyCap(label: String, active: Boolean = false, onClick: () -> Unit) {
    Surface(
        onClick = onClick,
        modifier = Modifier.widthIn(min = 48.dp),
        shape = RoundedCornerShape(8.dp),
        color = if (active) MaterialTheme.colorScheme.primary else MaterialTheme.colorScheme.surfaceVariant,
        contentColor = if (active) MaterialTheme.colorScheme.onPrimary else MaterialTheme.colorScheme.onSurfaceVariant,
    ) {
        Box(Modifier.padding(vertical = 10.dp), contentAlignment = Alignment.Center) {
            Text(label, fontFamily = FontFamily.Monospace, style = MaterialTheme.typography.labelLarge)
        }
    }
}

/**
 * Two-finger pinch → zoom. Only reacts (and consumes) while two pointers are
 * down, so single-finger scrolling underneath keeps working.
 */
private fun Modifier.pinchZoom(onZoom: (Float) -> Unit): Modifier = pointerInput(Unit) {
    awaitPointerEventScope {
        var prev = 0f
        while (true) {
            val event = awaitPointerEvent()
            val pts = event.changes.filter { it.pressed }
            if (pts.size >= 2) {
                val dist = (pts[0].position - pts[1].position).getDistance()
                if (prev > 0f && dist > 0f) {
                    val r = dist / prev
                    if (r.isFinite() && r > 0f) onZoom(r)
                }
                prev = dist
                pts.forEach { it.consume() }
            } else {
                prev = 0f
            }
        }
    }
}

private val TerminalBg = Color(0xFF0B0E14)
private val TerminalFg = Color(0xFFD7DCE5)

// Full Nerd Font Mono (Noto Sans M Nerd) — has the COMPLETE glyph set incl. the
// BigLinux logo (U+F347) and every powerline/devicon the ble.sh theme uses, so
// nothing renders as a missing-character box (the p10k Meslo subset lacked them).
private val TermFont = FontFamily(Font(R.font.nerd_mono))
