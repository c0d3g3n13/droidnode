package com.droidnode.exposers

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Intent
import android.os.IBinder
import android.util.Log
import com.droidnode.brokers.BatteryBroker
import com.droidnode.brokers.NetworkBroker
import com.droidnode.brokers.RustProcessBroker
import com.droidnode.brokers.WakeLockBroker
import com.droidnode.services.foundation.AgentLifecycleService
import com.droidnode.services.foundation.ResourceGuardService
import com.droidnode.services.orchestration.NodeReadinessService
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.launch
import java.io.File

private const val TAG = "ForegroundServiceExposer"
private const val NOTIFICATION_ID = 1001
private const val CHANNEL_ID = "droidnode_channel"

/**
 * Android Foreground Service entry point.
 * Responsible only for Android lifecycle concerns: notification, wake lock,
 * broker construction, and launching the readiness loop on a coroutine.
 * All execution logic lives in the Rust agent.
 */
class ForegroundServiceExposer : Service() {

    private val serviceScope = CoroutineScope(SupervisorJob() + Dispatchers.Default)

    private lateinit var wakeLockBroker: WakeLockBroker
    private lateinit var networkBroker: NetworkBroker
    private lateinit var nodeReadinessService: NodeReadinessService

    override fun onCreate() {
        super.onCreate()
        Log.i(TAG, "service creating")

        createNotificationChannel()

        // ─── Brokers ────────────────────────────────────────────────────────

        wakeLockBroker = WakeLockBroker(this)

        networkBroker = NetworkBroker(this).also { it.register() }

        val batteryBroker = BatteryBroker(this)

        val agentBinary = extractAsset("node-agent")
        extractAsset("proot") // proot is read by the Rust agent at DROIDNODE_PROOT_PATH

        val dataDir = filesDir
        val nodeId = "droidnode-${deviceFingerprint()}"
        val kubeConfig = File(dataDir, "kubeconfig")

        val processBroker = RustProcessBroker(
            binaryPath = agentBinary,
            dataDir = dataDir,
            kubeConfigPath = kubeConfig,
            nodeId = nodeId,
        )

        // ─── Services ───────────────────────────────────────────────────────

        val agentLifecycle = AgentLifecycleService(processBroker)
        val resourceGuard = ResourceGuardService(batteryBroker, networkBroker)
        nodeReadinessService = NodeReadinessService(agentLifecycle, resourceGuard)
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        Log.i(TAG, "service starting")
        startForeground(NOTIFICATION_ID, buildNotification())
        wakeLockBroker.acquire()

        serviceScope.launch {
            nodeReadinessService.runLoop()
        }

        return START_STICKY
    }

    override fun onDestroy() {
        super.onDestroy()
        Log.i(TAG, "service destroyed")
        serviceScope.cancel()
        networkBroker.unregister()
        wakeLockBroker.release()
    }

    override fun onBind(intent: Intent?): IBinder? = null

    // ─── Notification ────────────────────────────────────────────────────────

    private fun createNotificationChannel() {
        val channel = NotificationChannel(
            CHANNEL_ID,
            "DroidNode Agent",
            NotificationManager.IMPORTANCE_LOW
        ).apply {
            description = "DroidNode compute agent is running"
            setShowBadge(false)
        }
        getSystemService(NotificationManager::class.java).createNotificationChannel(channel)
    }

    private fun buildNotification(): Notification =
        Notification.Builder(this, CHANNEL_ID)
            .setContentTitle("DroidNode")
            .setContentText("Compute node active")
            .setSmallIcon(android.R.drawable.ic_dialog_info)
            .setOngoing(true)
            .build()

    // ─── Asset extraction ────────────────────────────────────────────────────

    /**
     * Copy a named binary from APK assets to internal storage on first run.
     * Returns the extracted executable File.
     */
    private fun extractAsset(name: String): File {
        val destFile = File(filesDir, name)
        if (!destFile.exists()) {
            Log.i(TAG, "extracting $name from assets")
            assets.open(name).use { input ->
                destFile.outputStream().use { output -> input.copyTo(output) }
            }
            destFile.setExecutable(true)
            Log.i(TAG, "$name extracted to ${destFile.absolutePath}")
        }
        return destFile
    }

    private fun deviceFingerprint(): String {
        return android.provider.Settings.Secure.getString(
            contentResolver,
            android.provider.Settings.Secure.ANDROID_ID
        ) ?: "unknown"
    }
}
