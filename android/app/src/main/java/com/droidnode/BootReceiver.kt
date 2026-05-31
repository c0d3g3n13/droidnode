package com.droidnode

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.os.Build
import android.util.Log
import com.droidnode.exposers.ForegroundServiceExposer

private const val TAG = "BootReceiver"

class BootReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        if (intent.action != Intent.ACTION_BOOT_COMPLETED) return
        Log.i(TAG, "boot completed — starting DroidNode agent")
        val svcIntent = Intent(context, ForegroundServiceExposer::class.java)
        context.startForegroundService(svcIntent)
    }
}
