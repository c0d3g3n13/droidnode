package com.droidnode.brokers

import android.util.Log
import java.io.File

private const val TAG = "RustProcessBroker"

/**
 * Broker: spawns, monitors, and kills the Rust node-agent subprocess.
 * No retry logic, no restart logic — that belongs in AgentLifecycleService.
 */
class RustProcessBroker(
    private val binaryPath: File,
    private val dataDir: File,
    private val kubeConfigPath: File,
    private val nodeId: String,
    private val kubeletPort: Int = 10250,
) {
    private var process: Process? = null

    /** Spawn the Rust binary and return the Process handle. Throws on failure. */
    fun spawn(): Process {
        if (!binaryPath.exists()) error("proot binary not found at ${binaryPath.absolutePath}")
        if (!binaryPath.canExecute()) binaryPath.setExecutable(true)

        val prootPath = File(dataDir, "proot")
        val layersDir = File(dataDir, "layers").also { it.mkdirs() }
        val rootfsDir = File(dataDir, "rootfs").also { it.mkdirs() }

        val pb = ProcessBuilder(binaryPath.absolutePath)
            .directory(dataDir)
            .redirectErrorStream(false)
            .apply {
                environment().apply {
                    put("DROIDNODE_NODE_ID", nodeId)
                    put("DROIDNODE_PROOT_PATH", prootPath.absolutePath)
                    put("DROIDNODE_LAYERS_DIR", layersDir.absolutePath)
                    put("DROIDNODE_ROOTFS_DIR", rootfsDir.absolutePath)
                    put("DROIDNODE_KUBELET_PORT", kubeletPort.toString())
                    put("KUBECONFIG", kubeConfigPath.absolutePath)
                    put("RUST_LOG", "info")
                }
            }

        val proc = pb.start()
        process = proc
        Log.i(TAG, "Rust agent started (pid=${proc.pid()})")
        return proc
    }

    /** Kill the current process immediately. Safe to call if not running. */
    fun kill() {
        process?.let {
            it.destroyForcibly()
            Log.i(TAG, "Rust agent killed")
        }
        process = null
    }

    /** Returns the exit value if the process has terminated, null if still running. */
    fun exitCode(): Int? {
        val p = process ?: return null
        return try {
            p.exitValue()
        } catch (_: IllegalThreadStateException) {
            null
        }
    }

    /** True if the agent process is alive. */
    fun isAlive(): Boolean = process?.isAlive == true
}
