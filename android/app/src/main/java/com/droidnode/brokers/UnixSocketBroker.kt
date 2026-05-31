package com.droidnode.brokers

import android.net.LocalSocket
import android.net.LocalSocketAddress
import android.util.Log
import java.io.Closeable
import java.io.InputStream
import java.io.OutputStream

private const val TAG = "UnixSocketBroker"

/**
 * Broker: opens and closes a Unix domain socket connection to the Rust agent.
 * No protocol, no message parsing — that is the caller's concern.
 */
class UnixSocketBroker(private val socketPath: String) : Closeable {

    private var socket: LocalSocket? = null

    /** Open the socket connection. Throws if the socket is unavailable. */
    fun connect() {
        val s = LocalSocket()
        s.connect(LocalSocketAddress(socketPath, LocalSocketAddress.Namespace.FILESYSTEM))
        socket = s
        Log.i(TAG, "connected to Unix socket at $socketPath")
    }

    /** Write raw bytes to the socket. */
    fun write(data: ByteArray) {
        outputStream().write(data)
        outputStream().flush()
    }

    /** Read up to `maxBytes` from the socket. Returns empty array on EOF. */
    fun read(maxBytes: Int = 4096): ByteArray {
        val buf = ByteArray(maxBytes)
        val n = inputStream().read(buf)
        return if (n <= 0) ByteArray(0) else buf.copyOf(n)
    }

    fun inputStream(): InputStream =
        socket?.inputStream ?: error("socket not connected")

    fun outputStream(): OutputStream =
        socket?.outputStream ?: error("socket not connected")

    fun isConnected(): Boolean = socket?.isConnected == true

    override fun close() {
        try {
            socket?.close()
        } catch (e: Exception) {
            Log.w(TAG, "socket close error: ${e.message}")
        }
        socket = null
        Log.i(TAG, "Unix socket closed")
    }
}
