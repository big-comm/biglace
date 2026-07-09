package org.communitybig.biglace.core.ssh

import androidx.lifecycle.ViewModel
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
)

/** Keeps SSH/SFTP tab metadata stable while each tab owns a separate client. */
class SshSessionTabsViewModel : ViewModel() {
    private var nextId = 1L
    private val first = newSession()

    private val _tabs = MutableStateFlow(listOf(first))
    val tabs = _tabs.asStateFlow()

    private val _activeId = MutableStateFlow(first.id)
    val activeId = _activeId.asStateFlow()

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

        val remaining = current.filterNot { it.id == id }.ifEmpty { listOf(newSession()) }
        _tabs.value = remaining
        if (_activeId.value == id) {
            _activeId.value = remaining[index.coerceAtMost(remaining.lastIndex)].id
        }
    }

    private fun newSession(
        host: String = "",
        user: String = "",
        password: String = "",
        fromPeer: Boolean = false,
    ) = SshSessionTab(nextId++, host = host, user = user, password = password, fromPeer = fromPeer)
}
