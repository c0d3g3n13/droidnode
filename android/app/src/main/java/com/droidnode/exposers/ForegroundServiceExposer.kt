package com.droidnode.exposers

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
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
import com.droidnode.ui.DebugActivity
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

    companion object {
        @Volatile var isAgentRunning: Boolean = false
        @Volatile var nodeId: String = ""
        @Volatile var startTime: Long = 0L
    }

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

        // Binaries are packaged as .so files in jniLibs/arm64-v8a/ so the
        // package manager extracts them to nativeLibraryDir with execute permission.
        // filesDir is mounted noexec on API 29+ so we cannot execute from there.
        val nativeDir = applicationInfo.nativeLibraryDir
        val agentBinary = File(nativeDir, "libnode_agent.so")
        val prootBinary = File(nativeDir, "libproot.so")

        if (!agentBinary.exists()) {
            Log.e(TAG, "node-agent binary not found at ${agentBinary.absolutePath} — APK may be missing jniLibs")
        }

        val dataDir = filesDir
        val nodeId = "droidnode-${deviceFingerprint()}"
        val kubeConfig = File(dataDir, "kubeconfig")

        val processBroker = RustProcessBroker(
            binaryPath = agentBinary,
            prootPath = prootBinary,
            dataDir = dataDir,
            cacheDir = cacheDir,
            codeCacheDir = codeCacheDir,
            nativeLibDir = nativeDir,
            kubeConfigPath = kubeConfig,
            nodeId = nodeId,
        )

        // Expose node ID globally for DebugActivity
        ForegroundServiceExposer.nodeId = nodeId

        // ─── Services ───────────────────────────────────────────────────────

        val agentLifecycle = AgentLifecycleService(processBroker) { running ->
            isAgentRunning = running
            if (running) startTime = System.currentTimeMillis()
        }
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

    private fun buildNotification(): Notification {
        val tapIntent = Intent(this, DebugActivity::class.java).apply {
            flags = Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TOP
        }
        val tapPending = PendingIntent.getActivity(
            this, 0, tapIntent,
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
        )
        return Notification.Builder(this, CHANNEL_ID)
            .setContentTitle("DroidNode")
            .setContentText("Compute node active — tap to debug")
            .setSmallIcon(android.R.drawable.ic_dialog_info)
            .setContentIntent(tapPending)
            .setOngoing(true)
            .build()
    }

    private fun deviceFingerprint(): String {
        return android.provider.Settings.Secure.getString(
            contentResolver,
            android.provider.Settings.Secure.ANDROID_ID
        ) ?: "unknown"
    }
}
