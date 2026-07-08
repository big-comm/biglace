package org.communitybig.biglace.core.data

import android.content.Context
import org.json.JSONObject

/**
 * Storage for secrets: the pre-auth key and the (optional) panel password.
 *
 * TODO(security, mobile/ARCHITECTURE.md §8): migrate to EncryptedSharedPreferences
 * backed by an Android Keystore master key before shipping. This scaffold uses a
 * plain (private-mode) SharedPreferences so the project builds with no extra
 * dependency; on a non-rooted device MODE_PRIVATE already isolates it per-app,
 * but at-rest encryption is the bar we want. The desktop client stores the key
 * in plaintext TOML — mobile can and should do better here.
 */
class SecretStore(context: Context) {
    private val prefs = context.getSharedPreferences("biglace_secrets", Context.MODE_PRIVATE)

    var authKey: String
        get() = prefs.getString(KEY_AUTHKEY, "").orEmpty()
        set(v) = prefs.edit().putString(KEY_AUTHKEY, v.trim()).apply()

    fun panelPassword(): String? = prefs.getString(KEY_PANEL_PW, null)

    fun setPanelPassword(pw: String) {
        prefs.edit().putString(KEY_PANEL_PW, pw).apply()
    }

    // ── SSH key (public-key auth) ────────────────────────────────────────────
    var sshPrivateKey: String
        get() = prefs.getString(KEY_SSH_PRIV, "").orEmpty()
        set(v) = prefs.edit().putString(KEY_SSH_PRIV, v).apply()

    var sshPublicKey: String
        get() = prefs.getString(KEY_SSH_PUB, "").orEmpty()
        set(v) = prefs.edit().putString(KEY_SSH_PUB, v).apply()

    fun hasSshKey(): Boolean = sshPrivateKey.isNotBlank()

    fun setSshKeys(privatePem: String, publicOpenSsh: String) {
        prefs.edit()
            .putString(KEY_SSH_PRIV, privatePem)
            .putString(KEY_SSH_PUB, publicOpenSsh)
            .apply()
    }

    // ── Per-host SSH passwords (so you don't retype them every time) ──────────
    fun sshPassword(host: String): String? =
        loadMap(KEY_SSH_PW).optString(host, "").ifBlank { null }

    fun setSshPassword(host: String, password: String) = putInMap(KEY_SSH_PW, host, password)

    // ── Per-host SSH usernames (the OS login differs from the peer hostname; we
    // can't guess it, so remember what worked and prefill it next time) ─────────
    fun sshUser(host: String): String? =
        loadMap(KEY_SSH_USER).optString(host, "").ifBlank { null }

    fun setSshUser(host: String, user: String) = putInMap(KEY_SSH_USER, host, user)

    private fun loadMap(key: String): JSONObject =
        runCatching { JSONObject(prefs.getString(key, "{}").orEmpty()) }.getOrDefault(JSONObject())

    private fun putInMap(key: String, host: String, value: String) {
        val obj = loadMap(key)
        if (value.isBlank()) obj.remove(host) else obj.put(host, value)
        prefs.edit().putString(key, obj.toString()).apply()
    }

    fun clear() {
        prefs.edit().clear().apply()
    }

    private companion object {
        const val KEY_AUTHKEY = "authkey"
        const val KEY_PANEL_PW = "panel_password"
        const val KEY_SSH_PRIV = "ssh_private_key"
        const val KEY_SSH_PUB = "ssh_public_key"
        const val KEY_SSH_PW = "ssh_passwords"
        const val KEY_SSH_USER = "ssh_users"
    }
}
