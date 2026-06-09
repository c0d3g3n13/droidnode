package com.droidnode

import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow

data class LogLine(
    val timeMs: Long = System.currentTimeMillis(),
    val text: String,
)

object LogBuffer {
    private const val MAX = 500
    private val buf = ArrayDeque<LogLine>()
    private val _flow = MutableStateFlow<List<LogLine>>(emptyList())
    val flow: StateFlow<List<LogLine>> get() = _flow

    @Synchronized
    fun append(text: String) {
        while (buf.size >= MAX) buf.removeFirst()
        buf.addLast(LogLine(text = text))
        _flow.value = buf.toList()
    }

    @Synchronized
    fun snapshot(): List<LogLine> = buf.toList()
}
