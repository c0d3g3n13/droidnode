package com.droidnode.brokers

import android.content.Context
import android.os.PowerManager
import android.util.Log

private const val TAG = "WakeLockBroker"

/**
 * Broker: acquires and releases an Android PARTIAL_WAKE_LOCK.
 * No logic — just wraps the PowerManager API.
 */
class WakeLockBroker(context: Context) {

    private val powerManager = context.getSystemService(Context.POWER_SERVICE) as PowerManager
    private var wakeLock: PowerManager.WakeLock? = null

    fun acquire() {
        if (wakeLock?.isHeld == true) return
        val wl = powerManager.newWakeLock(
            PowerManager.PARTIAL_WAKE_LOCK,
            "DroidNode::AgentWakeLock"
        )
        wl.acquire()
        wakeLock = wl
        Log.i(TAG, "wake lock acquired")
    }

    fun release() {
        wakeLock?.let {
            if (it.isHeld) {
                it.release()
                Log.i(TAG, "wake lock released")
            }
        }
        wakeLock = null
    }

    fun isHeld(): Boolean = wakeLock?.isHeld == true
}
