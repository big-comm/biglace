package org.communitybig.biglace.core.mesh

import android.content.Context
import community.biglace.tsbridge.Tsbridge
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.withContext
import org.json.JSONArray
import org.json.JSONObject
import org.communitybig.biglace.R
import java.net.NetworkInterface
import java.util.Collections

/**
 * REAL mesh backend: embeds Tailscale's userspace stack (tsnet) via the
 * `tsbridge` Go AAR. The phone joins the tailnet in-process (no device VPN),
 * and [StatusJSON][Tsbridge.statusJSON] yields the same `ipnstate.Status` shape
 * the desktop parses from `tailscale status --json` — so this shows the user's
 * ACTUAL network. SSH/SFTP reach tailnet-only peers through
 * [Tsbridge.forwardTo] (a local 127.0.0.1 port-forward into the tunnel).
 *
 * @param stateDir app-private, writable dir where tsnet persists node keys.
 */
class TsnetMeshBackend(context: Context, private val stateDir: String) : MeshBackend {
    private val appContext = context.applicationContext
    override val id = "tsnet"

    private val _state = MutableStateFlow<MeshState>(MeshState.Disconnected)
    override val state: StateFlow<MeshState> = _state.asStateFlow()

    private val _peers = MutableStateFlow<List<Peer>>(emptyList())
    override val peers: StateFlow<List<Peer>> = _peers.asStateFlow()
    private var refreshFailures = 0

    override suspend fun connect(server: String, authKey: String?, hostname: String) {
        require(server.isNotBlank()) { "Server URL is required" }
        refreshFailures = 0
        _state.value = MeshState.Connecting
        try {
            withContext(Dispatchers.IO) {
                updateInterfacesBlocking()
                Tsbridge.start(server.trim(), authKey?.trim().orEmpty(), hostname, stateDir)
            }
            refresh()
            if (_state.value is MeshState.Connecting) {
                _state.value = MeshState.Up(hostname.ifEmpty { "this-phone" }, null)
            }
        } catch (e: CancellationException) {
            throw e
        } catch (e: Throwable) {
            // Throwable, not Exception: a failed native-lib load throws
            // UnsatisfiedLinkError (an Error), and a Go panic surfaces here too;
            // showing them beats crashing the app.
            _state.value = MeshState.Error(describe(e))
        }
    }

    override suspend fun disconnect() {
        withContext(Dispatchers.IO) { Tsbridge.stop() }
        _state.value = MeshState.Disconnected
        _peers.value = emptyList()
        refreshFailures = 0
    }

    override suspend fun refresh() {
        if (!isRunning()) {
            if (_state.value != MeshState.Disconnected) {
                _peers.value = emptyList()
                _state.value = MeshState.Error(appContext.getString(R.string.mesh_engine_stopped))
            }
            return
        }
        try {
            val json = withContext(Dispatchers.IO) { Tsbridge.statusJSON() }
            val status = JSONObject(json)
            _peers.value = parsePeers(status)
            val self = status.optJSONObject("Self")
            val selfName = self?.let { firstLabel(it.optString("DNSName")) } ?: ""
            val selfIp = self?.optJSONArray("TailscaleIPs")?.optString(0)
            _state.value = MeshState.Up(selfName.ifEmpty { "this-phone" }, selfIp)
            refreshFailures = 0
        } catch (e: Throwable) {
            if (e is CancellationException) throw e
            refreshFailures++
            if (_state.value is MeshState.Connecting || refreshFailures >= 3) {
                _state.value = MeshState.Error(describe(e))
            }
        }
    }

    private fun describe(e: Throwable): String {
        val msg = e.message?.takeIf { it.isNotBlank() } ?: e.toString()
        val head = "${e.javaClass.simpleName}: $msg"
        // Append the tail of the engine log so the real cause is visible even
        // without a USB logcat.
        val logs = runCatching { Tsbridge.lastLogs() }.getOrDefault("")
            .lineSequence().filter { it.isNotBlank() }.toList().takeLast(6).joinToString("\n")
        return if (logs.isNotBlank()) "$head\n\n$logs" else head
    }

    /** Open a local port that tunnels to `host:port` over the tailnet. */
    suspend fun forward(host: String, port: Int): Int =
        withContext(Dispatchers.IO) {
            require(host.isNotBlank()) { "Host is required" }
            require(port in 1..65535) { "Port must be between 1 and 65535" }
            val address = if (':' in host && !host.startsWith('[')) "[$host]:$port" else "$host:$port"
            Tsbridge.forwardTo(address).toInt()
        }

    fun hasPersistedState(): Boolean = java.io.File(stateDir, "tailscaled.state").isFile

    suspend fun updateInterfaces() = withContext(Dispatchers.IO) { updateInterfacesBlocking() }

    /** True once the embedded engine is up (safe: never throws). */
    fun isRunning(): Boolean = runCatching { Tsbridge.running() }.getOrDefault(false)

    private fun parsePeers(status: JSONObject): List<Peer> {
        val peerMap = status.optJSONObject("Peer") ?: return emptyList()
        val users = status.optJSONObject("User")
        val out = ArrayList<Peer>()
        for (key in peerMap.keys()) {
            val n = peerMap.optJSONObject(key) ?: continue
            val dns = n.optString("DNSName").trimEnd('.')
            val suffix = status.optString("MagicDNSSuffix")
                .ifBlank { status.optJSONObject("CurrentTailnet")?.optString("MagicDNSSuffix").orEmpty() }
                .trimEnd('.')
            if (firstLabel(dns) == "panel" && suffix.isNotBlank() && dns.removePrefix("panel.") == suffix) continue
            val ips = n.optJSONArray("TailscaleIPs")
            var ipv4: String? = null
            var ipv6: String? = null
            if (ips != null) {
                for (i in 0 until ips.length()) {
                    val ip = ips.optString(i)
                    if (ip.contains(':')) { if (ipv6 == null) ipv6 = ip }
                    else if (ipv4 == null) ipv4 = ip
                }
            }
            val tags = ArrayList<String>()
            n.optJSONArray("Tags")?.let { for (i in 0 until it.length()) tags.add(it.optString(i).removePrefix("tag:")) }
            val sshUser = tags.firstOrNull { it.startsWith("user-") }
                ?.removePrefix("user-")
                ?.takeIf { it.isNotBlank() }
            out.add(
                Peer(
                    hostname = n.optString("HostName"),
                    ipv4 = ipv4,
                    ipv6 = ipv6,
                    dnsName = dns,
                    online = n.optBoolean("Online", false),
                    os = n.optString("OS"),
                    owner = resolveUser(users, n.optLong("UserID", 0)),
                    sshUser = sshUser,
                    lastSeen = n.optString("LastSeen").takeIf { it.isNotEmpty() && !it.startsWith("0001") },
                    tags = tags,
                    exitNodeOffered = n.optBoolean("ExitNodeOption", false),
                    exitNodeActive = n.optBoolean("ExitNode", false),
                ),
            )
        }
        out.sortWith(compareByDescending<Peer> { it.online }.thenBy { it.hostname })
        return out
    }

    private fun resolveUser(users: JSONObject?, uid: Long): String {
        if (users == null || uid <= 0) return ""
        val u = users.optJSONObject(uid.toString()) ?: return ""
        return u.optString("LoginName").substringBefore('@')
    }

    private fun firstLabel(dns: String): String = dns.trimEnd('.').substringBefore('.')

    /**
     * Snapshot the device's network interfaces via java.net.NetworkInterface
     * (which Android permits, unlike Go's net.Interfaces on API 30+) and hand
     * them to the Go engine as JSON. See [Tsbridge.setInterfaces].
     */
    private fun updateInterfacesBlocking() {
        Tsbridge.setInterfaces(interfacesJson())
    }

    private fun interfacesJson(): String {
        val arr = JSONArray()
        try {
            val ifaces = NetworkInterface.getNetworkInterfaces() ?: return "[]"
            for (ni in Collections.list(ifaces)) {
                val obj = JSONObject()
                obj.put("name", ni.name)
                obj.put("index", runCatching { ni.index }.getOrDefault(0))
                obj.put("mtu", runCatching { ni.mtu }.getOrDefault(1500))
                obj.put("up", runCatching { ni.isUp }.getOrDefault(false))
                obj.put("loopback", runCatching { ni.isLoopback }.getOrDefault(false))
                obj.put("p2p", runCatching { ni.isPointToPoint }.getOrDefault(false))
                obj.put("multicast", runCatching { ni.supportsMulticast() }.getOrDefault(false))
                val addrs = JSONArray()
                for (ia in ni.interfaceAddresses) {
                    val host = ia.address?.hostAddress?.substringBefore('%') ?: continue
                    addrs.put("$host/${ia.networkPrefixLength}")
                }
                obj.put("addrs", addrs)
                arr.put(obj)
            }
        } catch (e: Exception) {
            throw IllegalStateException("Unable to inspect Android network interfaces", e)
        }
        return arr.toString()
    }
}
