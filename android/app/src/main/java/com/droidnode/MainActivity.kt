package com.droidnode

import android.app.Activity
import android.content.Intent
import android.os.Bundle
import android.util.Log
import com.droidnode.exposers.ForegroundServiceExposer

private const val TAG = "MainActivity"

/**
 * Minimal launcher activity — just starts the foreground service and finishes.
 * All UI is the persistent notification; this activity is not shown after launch.
 */
class MainActivity : Activity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        Log.i(TAG, "launching DroidNode agent service")
        startForegroundService(Intent(this, ForegroundServiceExposer::class.java))
        finish()
    }
}
