package dev.phonecast.sender

import android.content.Context
import android.net.nsd.NsdManager
import android.net.nsd.NsdServiceInfo
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.update
import java.net.Inet4Address

/**
 * mDNS/DNS-SD discovery of receivers (docs/protocol.md "Discovery"):
 * browse `_phonecast._tcp.` and expose resolved endpoints to the UI.
 *
 * Hand-rolled NsdManager use because Compose state + callbacks don't mix
 * without one bridge object; the StateFlow is that bridge.
 */
class ReceiverDiscovery(private val context: Context) {
    data class Found(
        val name: String,
        val host: String,
        val tcpPort: Int,
        val udpPort: Int,
        val needsToken: Boolean,
    )

    val found = MutableStateFlow<Map<String, Found>>(emptyMap())

    private val nsd = context.getSystemService(Context.NSD_SERVICE) as NsdManager
    private var listener: NsdManager.DiscoveryListener? = null

    fun start() {
        if (listener != null) return
        val l = object : NsdManager.DiscoveryListener {
            override fun onStartDiscoveryFailed(serviceType: String, errorCode: Int) {
                // Common on some OEM ROMs right after unlock; the UI keeps
                // the manual address path working regardless.
                android.util.Log.w(TAG, "mDNS start failed: $errorCode")
            }

            override fun onStopDiscoveryFailed(serviceType: String, errorCode: Int) = Unit
            override fun onDiscoveryStarted(serviceType: String) = Unit
            override fun onDiscoveryStopped(serviceType: String) = Unit

            override fun onServiceFound(service: NsdServiceInfo) {
                nsd.resolveService(service, resolveListener())
            }

            override fun onServiceLost(service: NsdServiceInfo) {
                // Older platform versions may hand back a bare name here.
                val name = service.serviceName ?: service.host?.hostAddress
                name?.let { found.update { m -> m - it } }
            }
        }
        listener = l
        runCatching {
            nsd.discoverServices(SERVICE_TYPE, NsdManager.PROTOCOL_DNS_SD, l)
        }.onFailure { android.util.Log.w(TAG, "discoverServices threw", it) }
    }

    fun stop() {
        listener?.let { runCatching { nsd.stopServiceDiscovery(it) } }
        listener = null
    }

    private fun resolveListener() = object : NsdManager.ResolveListener {
        override fun onResolveFailed(si: NsdServiceInfo, errorCode: Int) = Unit
        override fun onServiceResolved(si: NsdServiceInfo) {
            val name = si.serviceName ?: return
            val host = (si.host as? Inet4Address)?.hostAddress ?: si.host?.hostAddress ?: return
            // TXT values arrive as byte arrays (API 25+ getters are typed
            // loosely across OEM implementations).
            fun txt(key: String): String? = si.attributes[key]
                ?.toString(Charsets.UTF_8)
                ?.trim('\u0000')
            val udp = txt("udp")?.toIntOrNull() ?: si.port + 1
            found.update {
                it + (name to Found(
                    name = name,
                    host = host,
                    tcpPort = si.port,
                    udpPort = udp,
                    needsToken = txt("auth") == "1",
                ))
            }
        }
    }

    companion object {
        private const val TAG = "PhoneCastDiscovery"
        const val SERVICE_TYPE = "_phonecast._tcp."
    }
}

/** Tiny SharedPreferences wrapper so the three fields survive restarts. */
class SavedSettings(context: Context) {
    private val prefs = context.getSharedPreferences("phonecast", Context.MODE_PRIVATE)

    var host: String
        get() = prefs.getString("host", "") ?: ""
        set(v) = prefs.edit().putString("host", v).apply()

    var tcpPort: String
        get() = prefs.getString("tcp", "53000") ?: "53000"
        set(v) = prefs.edit().putString("tcp", v).apply()

    var udpPort: String
        get() = prefs.getString("udp", "53001") ?: "53001"
        set(v) = prefs.edit().putString("udp", v).apply()

    var token: String
        get() = prefs.getString("token", "") ?: ""
        set(v) = prefs.edit().putString("token", v).apply()
}
