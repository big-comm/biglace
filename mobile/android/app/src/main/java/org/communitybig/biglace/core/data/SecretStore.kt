package org.communitybig.biglace.core.data

import android.content.Context
import org.json.JSONObject

/**
 * Storage for enrollment and SSH secrets.
 *
 * Values are encrypted with an AES-GCM key generated inside Android Keystore.
 * Existing plaintext preferences are migrated on first read.
 */
class SecretStore(context: Context) {
    private val prefs = SecurePreferences(context)

    var authKey: String
        get() = prefs.getString(KEY_AUTHKEY).orEmpty()
        set(v) = if (v.isBlank()) prefs.remove(KEY_AUTHKEY) else prefs.putString(KEY_AUTHKEY, v.trim())

    // ── SSH key (public-key auth) ────────────────────────────────────────────
    var sshPrivateKey: String
        get() = prefs.getString(KEY_SSH_PRIV).orEmpty()
        set(v) = if (v.isBlank()) prefs.remove(KEY_SSH_PRIV) else prefs.putString(KEY_SSH_PRIV, v)

    var sshPublicKey: String
        get() = prefs.getString(KEY_SSH_PUB).orEmpty()
        set(v) = if (v.isBlank()) prefs.remove(KEY_SSH_PUB) else prefs.putString(KEY_SSH_PUB, v)

    fun hasSshKey(): Boolean = sshPrivateKey.isNotBlank()

    fun setSshKeys(privatePem: String, publicOpenSsh: String) {
        prefs.putString(KEY_SSH_PRIV, privatePem)
        prefs.putString(KEY_SSH_PUB, publicOpenSsh)
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

    fun sshHostFingerprint(host: String): String? =
        loadMap(KEY_SSH_HOST_KEYS).optString(hostKey(host), "").ifBlank { null }

    fun setSshHostFingerprint(host: String, fingerprint: String) =
        putInMap(KEY_SSH_HOST_KEYS, hostKey(host), fingerprint)

    fun forgetSshHostFingerprint(host: String) = putInMap(KEY_SSH_HOST_KEYS, hostKey(host), "")

    fun clearSshHostFingerprints() = prefs.remove(KEY_SSH_HOST_KEYS)

    private fun hostKey(host: String): String = host.trim().lowercase()

    private fun loadMap(key: String): JSONObject =
        runCatching { JSONObject(prefs.getString(key).orEmpty().ifBlank { "{}" }) }
            .getOrDefault(JSONObject())

    private fun putInMap(key: String, host: String, value: String) {
        val obj = loadMap(key)
        if (value.isBlank()) obj.remove(host) else obj.put(host, value)
        prefs.putString(key, obj.toString())
    }

    fun clear() {
        prefs.clear()
    }

    private companion object {
        const val KEY_AUTHKEY = "authkey"
        const val KEY_SSH_PRIV = "ssh_private_key"
        const val KEY_SSH_PUB = "ssh_public_key"
        const val KEY_SSH_PW = "ssh_passwords"
        const val KEY_SSH_USER = "ssh_users"
        const val KEY_SSH_HOST_KEYS = "ssh_host_keys"
    }
}
