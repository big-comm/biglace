package org.communitybig.biglace.core.data

import android.annotation.SuppressLint
import android.content.Context
import android.content.SharedPreferences
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.Base64
import android.util.Log
import java.security.KeyStore
import javax.crypto.AEADBadTagException
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/** Small Android-Keystore-backed string store with transparent plaintext migration. */
@SuppressLint("ApplySharedPref", "UseKtx")
internal class SecurePreferences(context: Context) {
    private val prefs: SharedPreferences =
        context.getSharedPreferences(PREFS_NAME, Context.MODE_PRIVATE)

    // Decrypted values. Every read otherwise costs a Keystore round trip, and
    // these are read from the UI thread (form prefill, "can connect?" checks).
    private val cache = HashMap<String, String?>()

    @Synchronized
    fun getString(key: String): String? {
        if (cache.containsKey(key)) return cache[key]
        val stored = prefs.getString(key, null)
        if (stored == null) {
            cache[key] = null
            return null
        }
        if (!stored.startsWith(PREFIX)) {
            putString(key, stored)
            return stored
        }
        return try {
            val parts = stored.split(':', limit = 3)
            require(parts.size == 3) { "Invalid encrypted preference" }
            val cipher = Cipher.getInstance(TRANSFORMATION)
            cipher.init(
                Cipher.DECRYPT_MODE,
                secretKey(),
                GCMParameterSpec(TAG_BITS, Base64.decode(parts[1], Base64.NO_WRAP)),
            )
            cipher.doFinal(Base64.decode(parts[2], Base64.NO_WRAP)).toString(Charsets.UTF_8)
                .also { cache[key] = it }
        } catch (e: Exception) {
            // Only a value that can never be decrypted (corrupt, or its key is
            // gone) is dropped. A Keystore hiccup must not cost the user their
            // SSH key, so those are reported as "absent for now" and retried.
            if (e is AEADBadTagException || e is IllegalArgumentException) {
                Log.e(TAG, "Discarding unreadable encrypted preference: $key", e)
                prefs.edit().remove(key).apply()
                cache[key] = null
            } else {
                Log.e(TAG, "Keystore unavailable while reading: $key", e)
            }
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
        cache[key] = value
    }

    @Synchronized
    fun remove(key: String) {
        check(prefs.edit().remove(key).commit()) { "Failed to remove encrypted preference" }
        cache[key] = null
    }

    @Synchronized
    fun clear() {
        check(prefs.edit().clear().commit()) { "Failed to clear encrypted preferences" }
        cache.clear()
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
