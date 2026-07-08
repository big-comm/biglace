package org.communitybig.biglace.core.data

import android.content.Context
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import org.json.JSONObject

/**
 * Non-secret app settings, persisted in SharedPreferences (the framework store
 * — no extra dependency). Mirrors the desktop config.toml fields. Secrets (the
 * pre-auth key, panel password) live in [SecretStore], not here.
 *
 * `favorites` is exposed as a StateFlow so the peer list re-sorts reactively;
 * the rest are plain get/set read on demand by the settings screen.
 */
class SettingsStore(context: Context) {
    private val prefs = context.getSharedPreferences("biglace_settings", Context.MODE_PRIVATE)

    var serverUrl: String
        get() = prefs.getString(KEY_SERVER, "").orEmpty()
        set(v) = prefs.edit().putString(KEY_SERVER, v).apply()

    var hostname: String
        get() = prefs.getString(KEY_HOST, "").orEmpty()
        set(v) = prefs.edit().putString(KEY_HOST, v).apply()

    var panelUrl: String
        get() = prefs.getString(KEY_PANEL_URL, "").orEmpty()
        set(v) = prefs.edit().putString(KEY_PANEL_URL, v).apply()

    var panelUsername: String
        get() = prefs.getString(KEY_PANEL_USER, "").orEmpty()
        set(v) = prefs.edit().putString(KEY_PANEL_USER, v).apply()

    var autoConnect: Boolean
        get() = prefs.getBoolean(KEY_AUTO_CONNECT, false)
        set(v) = prefs.edit().putBoolean(KEY_AUTO_CONNECT, v).apply()

    /** Connect on device boot (via BootReceiver). */
    var connectOnBoot: Boolean
        get() = prefs.getBoolean(KEY_CONNECT_ON_BOOT, false)
        set(v) = prefs.edit().putBoolean(KEY_CONNECT_ON_BOOT, v).apply()

    private val _favorites = MutableStateFlow(
        prefs.getStringSet(KEY_FAVORITES, emptySet())!!.toSet(),
    )
    val favorites: StateFlow<Set<String>> = _favorites.asStateFlow()

    fun isFavorite(hostname: String): Boolean = _favorites.value.contains(hostname)

    fun toggleFavorite(hostname: String) {
        val next = _favorites.value.toMutableSet().apply {
            if (!add(hostname)) remove(hostname)
        }
        _favorites.value = next
        prefs.edit().putStringSet(KEY_FAVORITES, next).apply()
    }

    // ── Per-peer SSH login overrides (hostname → login), like desktop ─────────
    private val _overrides = MutableStateFlow(loadOverrides())
    val peerOverrides: StateFlow<Map<String, String>> = _overrides.asStateFlow()

    fun overrideFor(hostname: String): String? = _overrides.value[hostname]

    fun setOverride(hostname: String, login: String) {
        val next = _overrides.value.toMutableMap()
        if (login.isBlank()) next.remove(hostname) else next[hostname] = login.trim()
        _overrides.value = next
        val json = JSONObject()
        next.forEach { (k, v) -> json.put(k, v) }
        prefs.edit().putString(KEY_OVERRIDES, json.toString()).apply()
    }

    private fun loadOverrides(): Map<String, String> {
        val raw = prefs.getString(KEY_OVERRIDES, "").orEmpty()
        if (raw.isBlank()) return emptyMap()
        return runCatching {
            val obj = JSONObject(raw)
            buildMap { obj.keys().forEach { put(it, obj.optString(it)) } }
        }.getOrDefault(emptyMap())
    }

    private companion object {
        const val KEY_SERVER = "server_url"
        const val KEY_HOST = "hostname"
        const val KEY_PANEL_URL = "panel_url"
        const val KEY_PANEL_USER = "panel_username"
        const val KEY_AUTO_CONNECT = "auto_connect"
        const val KEY_CONNECT_ON_BOOT = "connect_on_boot"
        const val KEY_FAVORITES = "favorites"
        const val KEY_OVERRIDES = "peer_overrides"
    }
}
