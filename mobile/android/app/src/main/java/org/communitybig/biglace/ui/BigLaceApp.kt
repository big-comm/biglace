package org.communitybig.biglace.ui

import androidx.compose.foundation.layout.consumeWindowInsets
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.pager.HorizontalPager
import androidx.compose.foundation.pager.rememberPagerState
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Home
import androidx.compose.material.icons.filled.Settings
import androidx.compose.material3.Icon
import androidx.compose.material3.NavigationBar
import androidx.compose.material3.NavigationBarItem
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import kotlinx.coroutines.launch
import org.communitybig.biglace.AppContainer
import org.communitybig.biglace.R
import org.communitybig.biglace.feature.files.FilesScreen
import org.communitybig.biglace.feature.peers.PeersScreen
import org.communitybig.biglace.feature.settings.SettingsScreen
import org.communitybig.biglace.feature.terminal.TerminalScreen
import org.communitybig.biglace.ui.theme.BigLaceTheme

private enum class Tab(val labelRes: Int) {
    PEERS(R.string.tab_peers),
    TERMINAL(R.string.tab_terminal),
    FILES(R.string.tab_files),
    SETTINGS(R.string.tab_settings),
}

@Composable
private fun TabIcon(tab: Tab) = when (tab) {
    Tab.PEERS -> Icon(Icons.Filled.Home, contentDescription = null)
    Tab.TERMINAL -> Icon(painterResource(R.drawable.ic_terminal), contentDescription = null)
    Tab.FILES -> Icon(painterResource(R.drawable.ic_folder), contentDescription = null)
    Tab.SETTINGS -> Icon(Icons.Filled.Settings, contentDescription = null)
}

@Composable
fun BigLaceApp(container: AppContainer) {
    BigLaceTheme {
        val tabs = Tab.entries
        val pager = rememberPagerState(pageCount = { tabs.size })
        val scope = rememberCoroutineScope()
        fun goTo(tab: Tab) = scope.launch { pager.animateScrollToPage(tab.ordinal) }

        Scaffold(
            bottomBar = {
                NavigationBar {
                    tabs.forEach { t ->
                        NavigationBarItem(
                            selected = pager.currentPage == t.ordinal,
                            onClick = { goTo(t) },
                            icon = { TabIcon(t) },
                            label = { Text(stringResource(t.labelRes)) },
                        )
                    }
                }
            },
        ) { padding ->
            // Swipe left/right to switch screens; the bottom bar stays in sync.
            // padding = scaffold insets (incl. nav bar); consumeWindowInsets marks
            // them consumed so imePadding adds only (ime − navBar), not both — the
            // content shrinks to sit right above the keyboard with no leftover gap,
            // and the prompt stays visible instead of the window panning up.
            HorizontalPager(
                state = pager,
                modifier = Modifier.padding(padding).consumeWindowInsets(padding).imePadding(),
            ) { page ->
                when (tabs[page]) {
                    Tab.PEERS -> PeersScreen(
                        container = container,
                        onOpenTerminal = { goTo(Tab.TERMINAL) },
                        onOpenFiles = { goTo(Tab.FILES) },
                    )
                    Tab.TERMINAL -> TerminalScreen(container)
                    Tab.FILES -> FilesScreen(container)
                    Tab.SETTINGS -> SettingsScreen(container)
                }
            }
        }
    }
}
