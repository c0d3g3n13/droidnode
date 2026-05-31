package com.droidnode.services.orchestration

import android.util.Log
import com.droidnode.services.foundation.AgentLifecycleService
import com.droidnode.services.foundation.ResourceGuardService
import kotlinx.coroutines.delay

private const val TAG = "NodeReadinessService"
private const val MONITOR_INTERVAL_MS = 10_000L

/**
 * Orchestration service: combine battery + network + agent process state to decide
 * whether the node should be considered ready, and keep the agent alive.
 *
 * Call [runLoop] on a coroutine; it never returns unless cancelled.
 */
class NodeReadinessService(
    private val agentLifecycle: AgentLifecycleService,
    private val resourceGuard: ResourceGuardService,
) {
    suspend fun runLoop() {
        Log.i(TAG, "node readiness monitor started")

        while (true) {
            val readiness = resourceGuard.evaluate()

            if (!readiness.ready) {
                Log.w(TAG, "node not ready: ${readiness.reason}")
                if (agentLifecycle.isRunning()) {
                    Log.i(TAG, "stopping agent due to resource pressure")
                    agentLifecycle.stop()
                }
            } else {
                if (!agentLifecycle.isRunning()) {
                    Log.i(TAG, "resources recovered — starting agent")
                    try {
                        agentLifecycle.start()
                    } catch (e: Exception) {
                        Log.e(TAG, "failed to start agent: ${e.message}")
                    }
                } else {
                    // Agent is running — check if it died unexpectedly
                    val exitCode = agentLifecycle.exitCode()
                    if (exitCode != null) {
                        Log.w(TAG, "agent exited with code $exitCode — restarting")
                        try {
                            agentLifecycle.restart()
                        } catch (e: Exception) {
                            Log.e(TAG, "restart failed: ${e.message}")
                        }
                    }
                }
            }

            delay(MONITOR_INTERVAL_MS)
        }
    }

    fun isNodeReady(): Boolean {
        val readiness = resourceGuard.evaluate()
        return readiness.ready && agentLifecycle.isRunning()
    }
}
