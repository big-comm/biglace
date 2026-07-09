package org.communitybig.biglace.core.data

import android.annotation.SuppressLint
import android.content.Context
import android.content.SharedPreferences
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.Base64
import android.util.Log
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/** Small Android-Keystore-backed string store with transparent plaintext migration. */
@SuppressLint("ApplySharedPref", "UseKtx")
internal class SecurePreferences(context: Context) {
    private val prefs: SharedPreferences =
        context.getSharedPreferences(PREFS_NAME, Context.MODE_PRIVATE)

    @Synchronized
    fun getString(key: String): String? {
        val stored = prefs.getString(key, null) ?: return null
        if (!stored.startsWith(PREFIX)) {
            putString(key, stored)
            return stored
        }
        return runCatching {
            val parts = stored.split(':', limit = 3)
            require(parts.size == 3) { "Invalid encrypted preference" }
            val cipher = Cipher.getInstance(TRANSFORMATION)
            cipher.init(
                Cipher.DECRYPT_MODE,
                secretKey(),
                GCMParameterSpec(TAG_BITS, Base64.decode(parts[1], Base64.NO_WRAP)),
            )
            cipher.doFinal(Base64.decode(parts[2], Base64.NO_WRAP)).toString(Charsets.UTF_8)
        }.getOrElse {
            Log.e(TAG, "Discarding unreadable encrypted preference: $key", it)
            prefs.edit().remove(key).apply()
            null
        }
    }

    @Synchronized
    fun putString(key: String, value: String) {
        val cipher = Cipher.getInstance(TRANSFORMATION)
        cipher.init(Cipher.ENCRYPT_MODE, secretKey())
        val ciphertext = cipher.doFinal(value.toByteArray(Charsets.UTF_8))
        val encoded = buildString {
            append(PREFIX)
            append(Base64.encodeToString(cipher.iv, Base64.NO_WRAP))
            append(':')
            append(Base64.encodeToString(ciphertext, Base64.NO_WRAP))
        }
        check(prefs.edit().putString(key, encoded).commit()) { "Failed to persist encrypted preference" }
    }

    @Synchronized
    fun remove(key: String) {
        check(prefs.edit().remove(key).commit()) { "Failed to remove encrypted preference" }
    }

    @Synchronized
    fun clear() {
        check(prefs.edit().clear().commit()) { "Failed to clear encrypted preferences" }
    }

    private fun secretKey(): SecretKey {
        val store = KeyStore.getInstance(KEYSTORE).apply { load(null) }
        (store.getKey(KEY_ALIAS, null) as? SecretKey)?.let { return it }
        return KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, KEYSTORE).run {
            init(
                KeyGenParameterSpec.Builder(
                    KEY_ALIAS,
                    KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT,
                )
                    .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                    .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                    .setRandomizedEncryptionRequired(true)
                    .build(),
            )
            generateKey()
        }
    }

    private companion object {
        const val PREFS_NAME = "biglace_secrets"
        const val KEYSTORE = "AndroidKeyStore"
        const val KEY_ALIAS = "biglace-secrets-v1"
        const val TRANSFORMATION = "AES/GCM/NoPadding"
        const val TAG_BITS = 128
        const val PREFIX = "v1:"
        const val TAG = "BigLaceSecrets"
    }
}
