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
 *  - POST {base}/api/devices/me/os-user  {os_user}    (tailnet source-IP authed)
 *  - GET  {base}/api/devices/os-users → {hostname: os_user}     (same auth)
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

    /** Idempotent; the panel returns 304 when unchanged — treated as success. */
    suspend fun postOsUser(baseUrl: String, osUser: String) {
        withContext(Dispatchers.IO) {
            val body = JSONObject().put("os_user", osUser)
            postJson(
                "${baseUrl.trimEnd('/')}/api/devices/me/os-user",
                body,
                allow304 = true,
                allowTailnetHttp = true,
            )
        }
    }

    suspend fun fetchOsUsers(baseUrl: String): Map<String, String> =
        withContext(Dispatchers.IO) {
            val json = getJson("${baseUrl.trimEnd('/')}/api/devices/os-users")
            buildMap {
                json.keys().forEach { k -> put(k, json.optString(k)) }
            }
        }

    // ── HTTP helpers ────────────────────────────────────────────────────────

    private fun postJson(
        url: String,
        body: JSONObject,
        allow304: Boolean = false,
        allowTailnetHttp: Boolean = false,
    ): JSONObject {
        val conn = open(url, allowTailnetHttp).apply {
            requestMethod = "POST"
            doOutput = true
            setRequestProperty("Content-Type", "application/json")
            setRequestProperty("Accept", "application/json")
        }
        conn.outputStream.use { it.write(body.toString().toByteArray(Charsets.UTF_8)) }
        return readResponse(conn, allow304)
    }

    private fun getJson(url: String): JSONObject {
        val conn = open(url, allowTailnetHttp = true).apply {
            requestMethod = "GET"
            setRequestProperty("Accept", "application/json")
        }
        return readResponse(conn, allow304 = false)
    }

    private fun open(url: String, allowTailnetHttp: Boolean): HttpURLConnection {
        val parsed = URL(url)
        require(parsed.host.isNotBlank() && parsed.userInfo == null && parsed.query == null && parsed.ref == null) {
            text(R.string.panel_invalid_url)
        }
        val host = parsed.host.trim('[', ']').lowercase()
        val loopback = host == "localhost" || host == "127.0.0.1" || host == "::1"
        val tailnet = host.split('.').mapNotNull { it.toIntOrNull() }.let {
            it.size == 4 && it[0] == 100 && it[1] in 64..127
        } || host.startsWith("fd7a:115c:a1e0:")
        require(
            parsed.protocol == "https" ||
                (parsed.protocol == "http" && (loopback || (allowTailnetHttp && tailnet))),
        ) {
            text(R.string.panel_https_required)
        }
        return (parsed.openConnection() as HttpURLConnection).apply {
            connectTimeout = 10_000
            readTimeout = 30_000
            instanceFollowRedirects = false
        }
    }

    private fun readResponse(conn: HttpURLConnection, allow304: Boolean): JSONObject {
        try {
            val code = conn.responseCode
            if (code == HttpURLConnection.HTTP_NOT_MODIFIED && allow304) return JSONObject()
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
