package com.droidnode.services.foundation

import android.util.Log
import com.droidnode.LogBuffer
import com.droidnode.brokers.RustProcessBroker
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext

private const val TAG = "AgentLifecycleService"

class AgentLifecycleService(
    private val processBroker: RustProcessBroker,
    private val onStatusChange: ((running: Boolean) -> Unit)? = null,
) {

    private var agentProcess: Process? = null

    suspend fun start(): Process = withContext(Dispatchers.IO) {
        if (agentProcess?.isAlive == true) {
            Log.d(TAG, "agent already running")
            return@withContext agentProcess!!
        }
        val proc = processBroker.spawn()
        agentProcess = proc
        Log.i(TAG, "agent started")
        onStatusChange?.invoke(true)

        Thread {
            try {
                proc.inputStream.bufferedReader().forEachLine { line ->
                    Log.i("DroidNodeRust", line)
                    LogBuffer.append(line)
                }
            } catch (_: Exception) {}
            val code = try { proc.exitValue() } catch (_: Exception) { -1 }
            Log.w(TAG, "agent process ended, exit code=$code")
            LogBuffer.append("agent process ended (exit=$code)")
            onStatusChange?.invoke(false)
        }.also { it.isDaemon = true; it.name = "agent-log-reader" }.start()

        proc
    }

    suspend fun stop() = withContext(Dispatchers.IO) {
        processBroker.kill()
        agentProcess = null
        Log.i(TAG, "agent stopped")
        onStatusChange?.invoke(false)
    }

    suspend fun restart(): Process {
        stop()
        return start()
    }

    fun isRunning(): Boolean = processBroker.isAlive()

    fun exitCode(): Int? = processBroker.exitCode()
}
