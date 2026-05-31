package com.droidnode.services.foundation

import android.util.Log
import com.droidnode.brokers.RustProcessBroker
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext

private const val TAG = "AgentLifecycleService"

/**
 * Foundation service: single responsibility — start, stop, restart the Rust agent process.
 * Does not hold state about why it stopped; that is NodeReadinessService's concern.
 */
class AgentLifecycleService(private val processBroker: RustProcessBroker) {

    private var agentProcess: Process? = null

    /** Start the agent if not already running. Returns the process. */
    suspend fun start(): Process = withContext(Dispatchers.IO) {
        if (agentProcess?.isAlive == true) {
            Log.d(TAG, "agent already running")
            return@withContext agentProcess!!
        }
        val proc = processBroker.spawn()
        agentProcess = proc
        Log.i(TAG, "agent started")
        proc
    }

    /** Stop the agent immediately. */
    suspend fun stop() = withContext(Dispatchers.IO) {
        processBroker.kill()
        agentProcess = null
        Log.i(TAG, "agent stopped")
    }

    /** Stop then start. */
    suspend fun restart(): Process {
        stop()
        return start()
    }

    fun isRunning(): Boolean = processBroker.isAlive()

    fun exitCode(): Int? = processBroker.exitCode()
}
