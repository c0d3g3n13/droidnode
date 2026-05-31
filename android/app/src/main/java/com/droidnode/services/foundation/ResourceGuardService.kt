package com.droidnode.services.foundation

import com.droidnode.brokers.BatteryBroker
import com.droidnode.brokers.BatteryState
import com.droidnode.brokers.NetworkBroker
import com.droidnode.brokers.NetworkType

private const val LOW_BATTERY_THRESHOLD = 20
private const val TAG = "ResourceGuardService"

data class ResourceReadiness(
    val ready: Boolean,
    val batteryPressure: Boolean,
    val networkAvailable: Boolean,
    val reason: String,
)

/**
 * Foundation service: evaluate battery + network thresholds and return a readiness verdict.
 * Single responsibility — does not start/stop anything.
 */
class ResourceGuardService(
    private val batteryBroker: BatteryBroker,
    private val networkBroker: NetworkBroker,
) {
    fun evaluate(): ResourceReadiness {
        val battery: BatteryState = batteryBroker.readState()
        val networkType: NetworkType = networkBroker.networkType.value

        val batteryPressure = battery.percent < LOW_BATTERY_THRESHOLD && !battery.charging
        val networkAvailable = networkType != NetworkType.NONE

        val ready = !batteryPressure && networkAvailable

        val reason = when {
            batteryPressure -> "battery critically low (${battery.percent}%)"
            !networkAvailable -> "network unavailable"
            else -> "ok"
        }

        return ResourceReadiness(
            ready = ready,
            batteryPressure = batteryPressure,
            networkAvailable = networkAvailable,
            reason = reason,
        )
    }
}
