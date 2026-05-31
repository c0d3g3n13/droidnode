package com.droidnode.brokers

import android.content.Context
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.net.NetworkRequest
import android.util.Log
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow

private const val TAG = "NetworkBroker"

enum class NetworkType { WIFI, CELLULAR, ETHERNET, NONE }

/**
 * Broker: observes ConnectivityManager network callbacks.
 * Exposes a [networkType] StateFlow — no policy logic.
 */
class NetworkBroker(context: Context) {

    private val cm = context.getSystemService(Context.CONNECTIVITY_SERVICE) as ConnectivityManager

    private val _networkType = MutableStateFlow(currentType())
    val networkType: StateFlow<NetworkType> = _networkType

    private val callback = object : ConnectivityManager.NetworkCallback() {
        override fun onAvailable(network: Network) {
            _networkType.value = currentType()
            Log.i(TAG, "network available: ${_networkType.value}")
        }

        override fun onLost(network: Network) {
            _networkType.value = currentType()
            Log.i(TAG, "network lost: ${_networkType.value}")
        }

        override fun onCapabilitiesChanged(
            network: Network,
            caps: NetworkCapabilities
        ) {
            _networkType.value = caps.toNetworkType()
            Log.d(TAG, "network capabilities changed: ${_networkType.value}")
        }
    }

    fun register() {
        val req = NetworkRequest.Builder()
            .addCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET)
            .build()
        cm.registerNetworkCallback(req, callback)
        Log.i(TAG, "network callback registered")
    }

    fun unregister() {
        try {
            cm.unregisterNetworkCallback(callback)
        } catch (_: IllegalArgumentException) { }
        Log.i(TAG, "network callback unregistered")
    }

    private fun currentType(): NetworkType {
        val network = cm.activeNetwork ?: return NetworkType.NONE
        val caps = cm.getNetworkCapabilities(network) ?: return NetworkType.NONE
        return caps.toNetworkType()
    }

    private fun NetworkCapabilities.toNetworkType(): NetworkType = when {
        hasTransport(NetworkCapabilities.TRANSPORT_WIFI) -> NetworkType.WIFI
        hasTransport(NetworkCapabilities.TRANSPORT_ETHERNET) -> NetworkType.ETHERNET
        hasTransport(NetworkCapabilities.TRANSPORT_CELLULAR) -> NetworkType.CELLULAR
        else -> NetworkType.NONE
    }
}
