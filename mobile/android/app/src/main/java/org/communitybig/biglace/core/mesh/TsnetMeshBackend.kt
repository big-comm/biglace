package org.communitybig.biglace.core.mesh

import community.biglace.tsbridge.Tsbridge
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.withContext
import org.json.JSONArray
import org.json.JSONObject
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
class TsnetMeshBackend(private val stateDir: String) : MeshBackend {
    override val id = "tsnet"

    private val _state = MutableStateFlow<MeshState>(MeshState.Disconnected)
    override val state: StateFlow<MeshState> = _state.asStateFlow()

    private val _peers = MutableStateFlow<List<Peer>>(emptyList())
    override val peers: StateFlow<List<Peer>> = _peers.asStateFlow()

    override suspend fun connect(server: String, authKey: String?, hostname: String) {
        _state.value = MeshState.Connecting
        try {
            withContext(Dispatchers.IO) {
                // Android 11+ blocks Go's net.Interfaces(); feed the list from
                // Java first, or tsnet startup dies with "netlinkrib: permission
                // denied". MUST be before start().
                Tsbridge.setInterfaces(interfacesJson())
                Tsbridge.start(server, authKey ?: "", hostname, stateDir)
            }
            refresh()
            if (_state.value is MeshState.Connecting) {
                _state.value = MeshState.Up(hostname.ifEmpty { "this-phone" }, null)
            }
            // The netmap (peer list) can land a moment after we're Up. Re-poll a
            // few times so peers appear without the user tapping refresh.
            repeat(3) {
                delay(2500)
                if (_state.value is MeshState.Up) refresh()
            }
        } catch (e: Throwable) {
            // Throwable, not Exception: a failed native-lib load throws
            // UnsatisfiedLinkError (an Error), and a Go panic surfaces here too;
            // showing them beats crashing the app.
            _state.value = MeshState.Error(describe(e))
        }
    }

    override suspend fun disconnect() {
        withContext(Dispatchers.IO) { runCatching { Tsbridge.stop() } }
        _state.value = MeshState.Disconnected
        _peers.value = emptyList()
    }

    override suspend fun refresh() {
        if (!isRunning()) return
        try {
            val json = withContext(Dispatchers.IO) { Tsbridge.statusJSON() }
            val status = JSONObject(json)
            _peers.value = parsePeers(status)
            val self = status.optJSONObject("Self")
            val selfName = self?.let { firstLabel(it.optString("DNSName")) } ?: ""
            val selfIp = self?.optJSONArray("TailscaleIPs")?.optString(0)
            _state.value = MeshState.Up(selfName.ifEmpty { "this-phone" }, selfIp)
        } catch (e: Throwable) {
            _state.value = MeshState.Error(describe(e))
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
        withContext(Dispatchers.IO) { Tsbridge.forwardTo("$host:$port").toInt() }

    /** True once the embedded engine is up (safe: never throws). */
    fun isRunning(): Boolean = runCatching { Tsbridge.running() }.getOrDefault(false)

    private fun parsePeers(status: JSONObject): List<Peer> {
        val peerMap = status.optJSONObject("Peer") ?: return emptyList()
        val users = status.optJSONObject("User")
        val out = ArrayList<Peer>()
        for (key in peerMap.keys()) {
            val n = peerMap.optJSONObject(key) ?: continue
            val dns = n.optString("DNSName").trimEnd('.')
            // Skip the panel/infra peer, like the desktop does.
            if (firstLabel(dns) == "panel") continue
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
            out.add(
                Peer(
                    hostname = n.optString("HostName"),
                    ipv4 = ipv4,
                    ipv6 = ipv6,
                    dnsName = dns,
                    online = n.optBoolean("Online", false),
                    os = n.optString("OS"),
                    owner = resolveUser(users, n.optLong("UserID", 0)),
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
        } catch (_: Exception) {
        }
        return arr.toString()
    }
}
