package org.communitybig.biglace.core.ssh

import androidx.lifecycle.ViewModel
import androidx.lifecycle.ViewModelStore
import androidx.lifecycle.ViewModelStoreOwner
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update

data class SshSessionTab(
    val id: Long,
    val host: String = "",
    val port: String = "22",
    val user: String = "",
    val password: String = "",
    val mode: AuthMode = AuthMode.AUTO,
    val fromPeer: Boolean = false,
    /** Connect as soon as the tab is shown (set on a duplicated session). */
    val autoConnect: Boolean = false,
) {
    /** `user@host` (or just the host) — how a session is named to the user. */
    val label: String
        get() = if (user.isBlank()) host else "$user@$host"
}

/** Keeps SSH/SFTP tab metadata stable while each tab owns a separate client. */
class SshSessionTabsViewModel : ViewModel() {
    private var nextId = 1L
    private val first = newSession()

    private val _tabs = MutableStateFlow(listOf(first))
    val tabs = _tabs.asStateFlow()

    private val _activeId = MutableStateFlow(first.id)
    val activeId = _activeId.asStateFlow()

    private val owners = HashMap<Long, TabOwner>()

    private class TabOwner : ViewModelStoreOwner {
        override val viewModelStore = ViewModelStore()
    }

    /**
     * Where a tab's session ViewModel lives. Scoping it to the tab (instead of
     * the Activity) is what lets [close] actually free the session — terminal
     * buffer, SSH client and all — rather than leaking it until the app exits.
     */
    fun ownerFor(id: Long): ViewModelStoreOwner = owners.getOrPut(id) { TabOwner() }

    fun openPeer(host: String, user: String, password: String): Long {
        val existing = _tabs.value.firstOrNull {
            it.host.equals(host, ignoreCase = true) && it.user == user && it.port == "22"
        }
        if (existing != null) {
            if (existing.password.isBlank() && password.isNotBlank()) {
                update(existing.id) { it.copy(password = password, fromPeer = true) }
            }
            _activeId.value = existing.id
            return existing.id
        }

        val blank = _tabs.value.firstOrNull { it.host.isBlank() && it.user.isBlank() }
        val target = if (blank != null) {
            blank.copy(host = host, user = user, password = password, fromPeer = true)
                .also { replacement ->
                    _tabs.update { tabs -> tabs.map { if (it.id == blank.id) replacement else it } }
                }
        } else {
            newSession(host, user, password, fromPeer = true)
                .also { session -> _tabs.update { it + session } }
        }
        _activeId.value = target.id
        return target.id
    }

    /**
     * Open another session to the same machine: a new tab right after [id],
     * with the same host, port, login and auth mode, which connects on its
     * own. Returns null when [id] has no host to copy.
     */
    fun duplicate(id: Long): Long? {
        val source = _tabs.value.firstOrNull { it.id == id }?.takeIf { it.host.isNotBlank() } ?: return null
        val copy = source.copy(id = nextId++, autoConnect = true)
        _tabs.update { tabs ->
            val at = tabs.indexOfFirst { it.id == id } + 1
            tabs.take(at) + copy + tabs.drop(at)
        }
        _activeId.value = copy.id
        return copy.id
    }

    fun add(): Long {
        val blank = _tabs.value.firstOrNull { it.host.isBlank() && it.user.isBlank() }
        if (blank != null) {
            _activeId.value = blank.id
            return blank.id
        }
        return newSession().also { session ->
            _tabs.update { it + session }
            _activeId.value = session.id
        }.id
    }

    fun select(id: Long) {
        if (_tabs.value.any { it.id == id }) _activeId.value = id
    }

    fun update(id: Long, transform: (SshSessionTab) -> SshSessionTab) {
        _tabs.update { tabs -> tabs.map { if (it.id == id) transform(it) else it } }
    }

    fun close(id: Long) {
        val current = _tabs.value
        val index = current.indexOfFirst { it.id == id }
        if (index < 0) return

        owners.remove(id)?.viewModelStore?.clear()
        val remaining = current.filterNot { it.id == id }.ifEmpty { listOf(newSession()) }
        _tabs.value = remaining
        if (_activeId.value == id) {
            _activeId.value = remaining[index.coerceAtMost(remaining.lastIndex)].id
        }
    }

    override fun onCleared() {
        owners.values.forEach { it.viewModelStore.clear() }
        owners.clear()
    }

    private fun newSession(
        host: String = "",
        user: String = "",
        password: String = "",
        fromPeer: Boolean = false,
    ) = SshSessionTab(nextId++, host = host, user = user, password = password, fromPeer = fromPeer)
}
