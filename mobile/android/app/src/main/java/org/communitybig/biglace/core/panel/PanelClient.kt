package org.communitybig.biglace.core.panel

import android.content.Context
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import org.json.JSONObject
import java.io.IOException
import java.net.HttpURLConnection
import java.net.URL
import java.io.InputStream
import java.io.ByteArrayOutputStream
import org.communitybig.biglace.R

data class PanelCredentials(
    val url: String,
    val username: String,
    val password: String,
    val node: String,
    val hostname: String,
)

data class PreAuthResponse(
    val authKey: String,
    val serverUrl: String,
)

/**
 * BigScale panel HTTP client — the exact contract the desktop client uses
 * (src/panel.rs), so both clients are interchangeable against the same panel:
 *
 *  - POST {panel}/api/v1/preauth-key  {username,password,node_user,hostname}
 *      → {authkey, server_url}                                  (body-authed)
 *
 * Uses HttpURLConnection + org.json (both in the framework) to stay dependency-
 * free. Runs on Dispatchers.IO; callers await from a coroutine.
 */
class PanelClient(context: Context) {
    private val appContext = context.applicationContext

    suspend fun requestPreauth(creds: PanelCredentials): PreAuthResponse =
        withContext(Dispatchers.IO) {
            require(creds.username.length <= 256 && creds.password.length <= 4096) {
                text(R.string.panel_credentials_too_long)
            }
            require(creds.node.length <= 63 && creds.hostname.length <= 63) {
                text(R.string.panel_device_name_too_long)
            }
            val body = JSONObject()
                .put("username", creds.username)
                .put("password", creds.password)
                .put("node_user", creds.node)
                .put("hostname", creds.hostname)
            val json = postJson("${creds.url.trimEnd('/')}/api/v1/preauth-key", body)
            val response = PreAuthResponse(
                authKey = json.optString("authkey"),
                serverUrl = json.optString("server_url"),
            )
            require(response.authKey.isNotBlank()) { text(R.string.panel_missing_authkey) }
            response
        }

    // ── HTTP helpers ────────────────────────────────────────────────────────

    private fun postJson(url: String, body: JSONObject): JSONObject {
        val conn = open(url).apply {
            requestMethod = "POST"
            doOutput = true
            setRequestProperty("Content-Type", "application/json")
            setRequestProperty("Accept", "application/json")
        }
        conn.outputStream.use { it.write(body.toString().toByteArray(Charsets.UTF_8)) }
        return readResponse(conn)
    }

    private fun open(url: String): HttpURLConnection {
        val parsed = URL(url)
        require(parsed.host.isNotBlank() && parsed.userInfo == null && parsed.query == null && parsed.ref == null) {
            text(R.string.panel_invalid_url)
        }
        require(parsed.protocol == "https" || (parsed.protocol == "http" && isLoopbackHost(parsed.host))) {
            text(R.string.panel_https_required)
        }
        return (parsed.openConnection() as HttpURLConnection).apply {
            connectTimeout = 10_000
            readTimeout = 30_000
            instanceFollowRedirects = false
        }
    }

    private fun readResponse(conn: HttpURLConnection): JSONObject {
        try {
            val code = conn.responseCode
            if (code in 200..299) {
                val text = readLimited(conn.inputStream)
                return if (text.isBlank()) JSONObject() else JSONObject(text)
            }
            // Mirror the desktop: surface the panel's {"error": "..."} when present.
            val err = conn.errorStream?.let(::readLimited).orEmpty()
            val detail = runCatching { JSONObject(err).optString("error") }.getOrNull()
            throw IOException(detail?.takeIf { it.isNotBlank() } ?: "HTTP $code")
        } finally {
            conn.disconnect()
        }
    }

    private fun readLimited(input: InputStream): String = input.use { stream ->
        val out = ByteArrayOutputStream()
        val buffer = ByteArray(8192)
        var total = 0
        while (true) {
            val read = stream.read(buffer)
            if (read < 0) break
            total += read
            require(total <= MAX_RESPONSE_BYTES) { text(R.string.panel_response_too_large) }
            out.write(buffer, 0, read)
        }
        out.toString(Charsets.UTF_8.name())
    }

    private companion object {
        const val MAX_RESPONSE_BYTES = 1_048_576
    }

    private fun text(id: Int): String = appContext.getString(id)
}

/**
 * The only hosts a panel may be reached at over plain HTTP: this device itself.
 * Matched exactly — "127.0.0.1.example.com" is a public hostname, not loopback.
 * Kept in sync with res/xml/network_security_config.xml.
 */
internal fun isLoopbackHost(host: String): Boolean =
    host.equals("localhost", ignoreCase = true) || host == "127.0.0.1"
