package org.communitybig.biglace.core.panel

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import org.json.JSONObject
import java.io.IOException
import java.net.HttpURLConnection
import java.net.URL

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
class PanelClient {

    suspend fun requestPreauth(creds: PanelCredentials): PreAuthResponse =
        withContext(Dispatchers.IO) {
            val body = JSONObject()
                .put("username", creds.username)
                .put("password", creds.password)
                .put("node_user", creds.node)
                .put("hostname", creds.hostname)
            val json = postJson("${creds.url.trimEnd('/')}/api/v1/preauth-key", body)
            PreAuthResponse(
                authKey = json.optString("authkey"),
                serverUrl = json.optString("server_url"),
            )
        }

    /** Idempotent; the panel returns 304 when unchanged — treated as success. */
    suspend fun postOsUser(baseUrl: String, osUser: String) {
        withContext(Dispatchers.IO) {
            val body = JSONObject().put("os_user", osUser)
            postJson("${baseUrl.trimEnd('/')}/api/devices/me/os-user", body, allow304 = true)
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

    private fun postJson(url: String, body: JSONObject, allow304: Boolean = false): JSONObject {
        val conn = open(url).apply {
            requestMethod = "POST"
            doOutput = true
            setRequestProperty("Content-Type", "application/json")
            setRequestProperty("Accept", "application/json")
        }
        conn.outputStream.use { it.write(body.toString().toByteArray()) }
        return readResponse(conn, allow304)
    }

    private fun getJson(url: String): JSONObject {
        val conn = open(url).apply {
            requestMethod = "GET"
            setRequestProperty("Accept", "application/json")
        }
        return readResponse(conn, allow304 = false)
    }

    private fun open(url: String): HttpURLConnection =
        (URL(url).openConnection() as HttpURLConnection).apply {
            connectTimeout = 10_000
            readTimeout = 30_000
        }

    private fun readResponse(conn: HttpURLConnection, allow304: Boolean): JSONObject {
        try {
            val code = conn.responseCode
            if (code == HttpURLConnection.HTTP_NOT_MODIFIED && allow304) return JSONObject()
            if (code in 200..299) {
                val text = conn.inputStream.bufferedReader().use { it.readText() }
                return if (text.isBlank()) JSONObject() else JSONObject(text)
            }
            // Mirror the desktop: surface the panel's {"error": "..."} when present.
            val err = conn.errorStream?.bufferedReader()?.use { it.readText() }.orEmpty()
            val detail = runCatching { JSONObject(err).optString("error") }.getOrNull()
            throw IOException(detail?.takeIf { it.isNotBlank() } ?: "HTTP $code")
        } finally {
            conn.disconnect()
        }
    }
}
