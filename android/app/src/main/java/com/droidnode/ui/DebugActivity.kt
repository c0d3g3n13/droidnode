package com.droidnode.ui

import android.app.Activity
import android.graphics.Color
import android.graphics.Typeface
import android.os.Bundle
import android.text.SpannableStringBuilder
import android.text.style.ForegroundColorSpan
import android.view.View
import android.view.WindowManager
import android.widget.ScrollView
import android.widget.TextView
import com.droidnode.LogBuffer
import com.droidnode.LogLine
import com.droidnode.R
import com.droidnode.exposers.ForegroundServiceExposer
import kotlinx.coroutines.*
import java.text.SimpleDateFormat
import java.util.*

class DebugActivity : Activity() {

    private val scope = CoroutineScope(Dispatchers.Main + SupervisorJob())

    private lateinit var tvAgentStatus: TextView
    private lateinit var tvNodeId: TextView
    private lateinit var tabLogs: TextView
    private lateinit var tabStatus: TextView
    private lateinit var tabPods: TextView
    private lateinit var scrollLogs: ScrollView
    private lateinit var tvLogs: TextView
    private lateinit var scrollStatus: ScrollView
    private lateinit var tvStatus: TextView
    private lateinit var scrollPods: ScrollView
    private lateinit var tvPods: TextView

    private val timeFmt = SimpleDateFormat("HH:mm:ss", Locale.US)
    private var activeTab = Tab.LOGS

    private enum class Tab { LOGS, STATUS, PODS }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        setContentView(R.layout.activity_debug)

        tvAgentStatus = findViewById(R.id.tvAgentStatus)
        tvNodeId      = findViewById(R.id.tvNodeId)
        tabLogs       = findViewById(R.id.tabLogs)
        tabStatus     = findViewById(R.id.tabStatus)
        tabPods       = findViewById(R.id.tabPods)
        scrollLogs    = findViewById(R.id.scrollLogs)
        tvLogs        = findViewById(R.id.tvLogs)
        scrollStatus  = findViewById(R.id.scrollStatus)
        tvStatus      = findViewById(R.id.tvStatus)
        scrollPods    = findViewById(R.id.scrollPods)
        tvPods        = findViewById(R.id.tvPods)

        tvLogs.typeface   = Typeface.MONOSPACE
        tvStatus.typeface = Typeface.MONOSPACE
        tvPods.typeface   = Typeface.MONOSPACE

        tabLogs.setOnClickListener   { selectTab(Tab.LOGS) }
        tabStatus.setOnClickListener { selectTab(Tab.STATUS) }
        tabPods.setOnClickListener   { selectTab(Tab.PODS) }

        selectTab(Tab.LOGS)

        // Stream log updates
        scope.launch {
            LogBuffer.flow.collect { lines ->
                updateHeader()
                when (activeTab) {
                    Tab.LOGS -> renderLogs(lines)
                    Tab.PODS -> renderPods(lines)
                    Tab.STATUS -> Unit
                }
            }
        }

        // Periodic refresh for STATUS tab and header badge
        scope.launch {
            while (isActive) {
                updateHeader()
                if (activeTab == Tab.STATUS) renderStatus()
                delay(2_000)
            }
        }
    }

    override fun onDestroy() {
        super.onDestroy()
        scope.cancel()
    }

    // ── Tab switching ────────────────────────────────────────────────────────

    private fun selectTab(tab: Tab) {
        activeTab = tab

        val bgActive   = Color.parseColor("#0D1117")
        val bgInactive = Color.parseColor("#161B22")
        val fgActive   = Color.parseColor("#58A6FF")
        val fgInactive = Color.parseColor("#8B949E")

        for ((view, t) in listOf(tabLogs to Tab.LOGS, tabStatus to Tab.STATUS, tabPods to Tab.PODS)) {
            view.setBackgroundColor(if (t == tab) bgActive else bgInactive)
            view.setTextColor(if (t == tab) fgActive else fgInactive)
        }

        scrollLogs.visibility   = if (tab == Tab.LOGS)   View.VISIBLE else View.GONE
        scrollStatus.visibility = if (tab == Tab.STATUS) View.VISIBLE else View.GONE
        scrollPods.visibility   = if (tab == Tab.PODS)   View.VISIBLE else View.GONE

        val lines = LogBuffer.snapshot()
        when (tab) {
            Tab.LOGS   -> renderLogs(lines)
            Tab.STATUS -> renderStatus()
            Tab.PODS   -> renderPods(lines)
        }
    }

    // ── Header ───────────────────────────────────────────────────────────────

    private fun updateHeader() {
        val running = ForegroundServiceExposer.isAgentRunning
        tvAgentStatus.text = if (running) "● RUNNING" else "● STOPPED"
        tvAgentStatus.setTextColor(Color.parseColor(if (running) "#3FB950" else "#F85149"))

        val id = ForegroundServiceExposer.nodeId
        tvNodeId.text = if (id.isNotEmpty()) "node: $id" else "node: starting..."
    }

    // ── LOGS tab ─────────────────────────────────────────────────────────────

    private fun renderLogs(lines: List<LogLine>) {
        val cDefault  = Color.parseColor("#E6EDF3")
        val cError    = Color.parseColor("#F85149")
        val cWarn     = Color.parseColor("#D29922")
        val cGreen    = Color.parseColor("#3FB950")
        val cDim      = Color.parseColor("#6E7681")

        val sb = SpannableStringBuilder()
        // Show last 200 lines to keep rendering fast
        for (line in lines.takeLast(200)) {
            val timeStr = "[${timeFmt.format(Date(line.timeMs))}] "
            val tsStart = sb.length
            sb.append(timeStr)
            sb.setSpan(ForegroundColorSpan(cDim), tsStart, sb.length, 0)

            val txtStart = sb.length
            sb.append(line.text).append('\n')
            val color = when {
                line.text.contains("ERROR",     ignoreCase = true) -> cError
                line.text.contains("FAILED",    ignoreCase = true) -> cError
                line.text.contains("WARN",      ignoreCase = true) -> cWarn
                line.text.contains("PASSED",    ignoreCase = true) -> cGreen
                line.text.contains("Succeeded", ignoreCase = false) -> cGreen
                line.text.contains("started",   ignoreCase = false) -> cGreen
                else -> cDefault
            }
            sb.setSpan(ForegroundColorSpan(color), txtStart, sb.length - 1, 0)
        }

        tvLogs.text = sb
        scrollLogs.post { scrollLogs.fullScroll(View.FOCUS_DOWN) }
    }

    // ── STATUS tab ───────────────────────────────────────────────────────────

    private fun renderStatus() {
        val running   = ForegroundServiceExposer.isAgentRunning
        val startTime = ForegroundServiceExposer.startTime
        val nodeId    = ForegroundServiceExposer.nodeId

        val uptime = if (startTime > 0L) {
            val diff = System.currentTimeMillis() - startTime
            val m = diff / 60_000
            val s = (diff / 1_000) % 60
            "${m}m ${s}s"
        } else "—"

        val logCount = LogBuffer.snapshot().size

        tvStatus.text = buildString {
            appendLine("┌─ Agent ────────────────────────────")
            appendLine("│  status  : ${if (running) "RUNNING ●" else "STOPPED ○"}")
            appendLine("│  uptime  : $uptime")
            appendLine("│  node id : ${nodeId.ifEmpty { "—" }}")
            appendLine("└────────────────────────────────────")
            appendLine()
            appendLine("┌─ Log buffer ───────────────────────")
            appendLine("│  lines   : $logCount / 500")
            appendLine("└────────────────────────────────────")
        }
    }

    // ── PODS tab ─────────────────────────────────────────────────────────────

    private fun renderPods(lines: List<LogLine>) {
        data class PodInfo(
            val name: String,
            var image: String = "",
            var phase: String = "Pending",
            var lastSeen: Long = 0L,
        )

        val pods = LinkedHashMap<String, PodInfo>()

        val reAssign   = Regex("""pod=Some\("([^"]+)"\).*image=(\S+)""")
        val reStarted  = Regex("""workload started pod=(\S+)""")
        val reFinished = Regex("""workload finished pod=(\S+) phase=(\S+)""")
        val reFailed   = Regex("""image preparation failed pod=Some\("([^"]+)"\)""")

        for (line in lines) {
            reAssign.find(line.text)?.let {
                val name  = it.groupValues[1]
                val image = it.groupValues[2]
                pods.getOrPut(name) { PodInfo(name) }.also { p ->
                    p.image = image; p.phase = "Pending"; p.lastSeen = line.timeMs
                }
                return@let
            }
            reStarted.find(line.text)?.let {
                val name = it.groupValues[1]
                pods.getOrPut(name) { PodInfo(name) }.also { p ->
                    p.phase = "Running"; p.lastSeen = line.timeMs
                }
                return@let
            }
            reFinished.find(line.text)?.let {
                val name  = it.groupValues[1]
                val phase = it.groupValues[2]
                pods.getOrPut(name) { PodInfo(name) }.also { p ->
                    p.phase = phase; p.lastSeen = line.timeMs
                }
                return@let
            }
            reFailed.find(line.text)?.let {
                val name = it.groupValues[1]
                pods.getOrPut(name) { PodInfo(name) }.also { p ->
                    p.phase = "Failed"; p.lastSeen = line.timeMs
                }
            }
        }

        if (pods.isEmpty()) {
            tvPods.text = "No pods observed yet.\n\nPods will appear here once\nscheduled to this node."
            return
        }

        tvPods.text = buildString {
            for (pod in pods.values.toList().takeLast(30)) {
                val icon = when (pod.phase) {
                    "Running"   -> "●"
                    "Succeeded" -> "✓"
                    "Failed"    -> "✗"
                    else        -> "◌"
                }
                appendLine("$icon  ${pod.name}")
                if (pod.image.isNotEmpty()) appendLine("   image : ${pod.image}")
                appendLine("   phase : ${pod.phase}")
                if (pod.lastSeen > 0L)
                    appendLine("   last  : ${timeFmt.format(Date(pod.lastSeen))}")
                appendLine()
            }
        }
    }
}
