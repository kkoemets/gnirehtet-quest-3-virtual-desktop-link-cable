package com.genymobile.gnirehtet.v4

import android.util.Log
import java.io.Closeable
import java.io.IOException
import java.net.InetSocketAddress
import java.net.Socket
import java.net.SocketTimeoutException
import java.nio.charset.StandardCharsets
import java.util.UUID
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicLong
import java.util.concurrent.atomic.AtomicReference
import kotlin.math.min

internal fun nextControlReconnectDelayMs(current: Long): Long {
    require(current >= 0) { "current delay must not be negative" }
    return min(current.saturatingMultiplyByTwo(), 1_000L)
}

private fun Long.saturatingMultiplyByTwo(): Long =
    if (this > Long.MAX_VALUE / 2) Long.MAX_VALUE else this * 2

class ControlSupervisor(
    private val sessionId: UUID,
    private val port: Int,
    private val listener: Listener,
    private val metricsProvider: () -> Gnr4Metrics? = { null },
) : Closeable {
    interface Listener {
        fun shouldReportStarted(): Boolean

        fun onControlConnected()

        fun onControlDegraded(error: Exception?)

        fun onControlTransportLost(error: Exception?) = Unit

        fun onControlRttSample(rttNanos: Long)

        fun onControlStopRequested(sendStopped: () -> Unit)
    }

    private val running = AtomicBoolean()
    private val paused = AtomicBoolean()
    private val sequence = AtomicLong()
    private val writeLock = Any()
    private val pauseLock = Object()
    private val suspendAcknowledgement = AtomicReference<CountDownLatch?>()
    private val controlReady = AtomicBoolean()
    private val recoveryReady = AtomicBoolean()
    private val stopRequested = AtomicBoolean()
    private val wakePending = AtomicBoolean()
    private val resetReconnectDelay = AtomicBoolean()
    @Volatile private var socket: Socket? = null
    @Volatile private var thread: Thread? = null

    init {
        require(port in 1_024..65_535) { "control port is outside 1024..65535" }
    }

    fun start(startPaused: Boolean = false) {
        synchronized(pauseLock) {
            if (!running.compareAndSet(false, true)) return
            paused.set(startPaused || paused.get())
            thread = Thread(::runLoop, "gnr4-control").apply { start() }
        }
    }

    fun suspend() {
        var connected: Socket? = null
        var acknowledgement: CountDownLatch? = null
        var acknowledged = false
        synchronized(pauseLock) {
            if (!paused.compareAndSet(false, true)) return
            connected = socket
            if (controlReady.get() && connected != null && !connected!!.isClosed) {
                acknowledgement = CountDownLatch(1)
                suspendAcknowledgement.set(acknowledgement)
            }
        }
        if (acknowledgement != null) {
            runCatching {
                connected!!.soTimeout = WAKE_CONFIRM_TIMEOUT_MS
                write(connected!!, Gnr4Frame(Gnr4MessageType.SUSPEND, sessionId))
            }.onFailure {
                suspendAcknowledgement.compareAndSet(acknowledgement, null)
                acknowledgement = null
            }
        }
        try {
            acknowledged = acknowledgement?.await(
                SUSPEND_ACK_TIMEOUT_MS,
                TimeUnit.MILLISECONDS,
            ) == true
        } catch (_: InterruptedException) {
            Thread.currentThread().interrupt()
        }
        suspendAcknowledgement.compareAndSet(acknowledgement, null)
        if (!stopRequested.get() && !acknowledged) connected?.close()
    }

    fun resume() {
        var connected: Socket? = null
        synchronized(pauseLock) {
            if (!paused.compareAndSet(true, false)) return
            wakePending.set(true)
            resetReconnectDelay.set(true)
            connected = socket?.takeIf {
                controlReady.get() && !stopRequested.get() && !it.isClosed
            }
            pauseLock.notifyAll()
        }
        val reusable = connected ?: return
        runCatching {
            reusable.soTimeout = WAKE_CONFIRM_TIMEOUT_MS
            write(
                reusable,
                Gnr4Frame(
                    Gnr4MessageType.STARTED,
                    sessionId,
                    Gnr4.startedPayload(wake = true),
                ),
            )
        }.onFailure {
            reusable.close()
        }
    }

    private fun runLoop() {
        var delayMs = RECONNECT_INITIAL_DELAY_MS
        while (running.get()) {
            if (!awaitResume()) return
            if (resetReconnectDelay.getAndSet(false)) {
                delayMs = WAKE_RECONNECT_DELAY_MS
            }
            try {
                Socket().use { connected ->
                    synchronized(pauseLock) {
                        if (!running.get() || paused.get()) return@use
                        socket = connected
                    }
                    connected.connect(InetSocketAddress(IPV4_LOOPBACK, port), CONNECT_TIMEOUT_MS)
                    connected.tcpNoDelay = true
                    connected.soTimeout = CONTROL_READ_TIMEOUT_MS
                    synchronized(pauseLock) {
                        if (!isActiveSocket(connected)) return@use
                        val hello = Gnr4.HELLO_CAPABILITIES.toByteArray(StandardCharsets.UTF_8)
                        write(connected, Gnr4Frame(Gnr4MessageType.HELLO, sessionId, hello))
                    }
                    val acknowledgement = Gnr4.read(connected.getInputStream(), sessionId)
                    if (acknowledgement.type != Gnr4MessageType.HELLO_ACK) {
                        throw IllegalStateException("Expected HELLO_ACK, got ${acknowledgement.type}")
                    }
                    val metricsEnabled = Gnr4.helloAckSupportsMetrics(acknowledgement.payload)
                    if (!listener.shouldReportStarted()) return@use
                    synchronized(pauseLock) {
                        if (!isActiveSocket(connected)) return@use
                        val wake = wakePending.get()
                        write(
                            connected,
                            Gnr4Frame(
                                Gnr4MessageType.STARTED,
                                sessionId,
                                Gnr4.startedPayload(wake),
                            ),
                        )
                        controlReady.set(true)
                        if (wake) {
                            connected.soTimeout = WAKE_CONFIRM_TIMEOUT_MS
                        } else {
                            recoveryReady.set(true)
                            listener.onControlConnected()
                        }
                    }
                    delayMs = RECONNECT_INITIAL_DELAY_MS
                    var missed = 0
                    val outstandingHeartbeats = linkedMapOf<Long, Long>()
                    var lastMetricsAt = System.nanoTime()
                    while (running.get() && !connected.isClosed) {
                        try {
                            val frame = Gnr4.read(connected.getInputStream(), sessionId)
                            when (frame.type) {
                                Gnr4MessageType.HEARTBEAT -> {
                                    missed = 0
                                    val heartbeat = Gnr4.parseHeartbeatPayload(frame.payload)
                                        ?: throw IOException("HEARTBEAT payload must be exactly 16 bytes")
                                    val sentAt = outstandingHeartbeats[heartbeat.sequence]
                                    if (sentAt == heartbeat.monotonicNanos) {
                                        outstandingHeartbeats.remove(heartbeat.sequence)
                                        listener.onControlRttSample(System.nanoTime() - sentAt)
                                    } else {
                                        write(
                                            connected,
                                            Gnr4Frame(Gnr4MessageType.HEARTBEAT, sessionId, frame.payload),
                                        )
                                    }
                                }
                                Gnr4MessageType.STATUS -> {
                                    missed = 0
                                    if (!paused.get() && wakePending.compareAndSet(true, false)) {
                                        resetReconnectDelay.set(false)
                                        connected.soTimeout = CONTROL_READ_TIMEOUT_MS
                                        recoveryReady.set(true)
                                        listener.onControlConnected()
                                    }
                                }
                                Gnr4MessageType.SUSPENDED -> {
                                    missed = 0
                                    suspendAcknowledgement.getAndSet(null)?.countDown()
                                }
                                Gnr4MessageType.STOP -> {
                                    stopRequested.set(true)
                                    suspendAcknowledgement.getAndSet(null)?.countDown()
                                    val acknowledgementSent = CountDownLatch(1)
                                    val acknowledgementClaimed = AtomicBoolean()
                                    listener.onControlStopRequested {
                                        if (acknowledgementClaimed.compareAndSet(false, true)) {
                                            try {
                                                write(connected, Gnr4Frame(Gnr4MessageType.STOPPED, sessionId))
                                            } finally {
                                                acknowledgementSent.countDown()
                                            }
                                        }
                                    }
                                    acknowledgementSent.await(STOP_ACK_TIMEOUT_MS, TimeUnit.MILLISECONDS)
                                    return
                                }
                                Gnr4MessageType.ERROR -> throw IllegalStateException("Host reported a control error")
                                else -> Unit
                            }
                        } catch (_: SocketTimeoutException) {
                            if (paused.get()) continue
                            if (wakePending.get()) {
                                throw SocketTimeoutException("Wake confirmation timed out")
                            }
                            val heartbeatSequence = sequence.incrementAndGet()
                            val sentAt = System.nanoTime()
                            while (outstandingHeartbeats.size >= MAX_OUTSTANDING_HEARTBEATS) {
                                outstandingHeartbeats.remove(outstandingHeartbeats.keys.first())
                            }
                            outstandingHeartbeats[heartbeatSequence] = sentAt
                            write(
                                connected,
                                Gnr4Frame(
                                    Gnr4MessageType.HEARTBEAT,
                                    sessionId,
                                    Gnr4.heartbeatPayload(heartbeatSequence, sentAt),
                                ),
                            )
                            missed += 1
                            if (missed >= 3) throw SocketTimeoutException("Three host heartbeats missed")
                        }
                        if (metricsEnabled && !paused.get()) {
                            lastMetricsAt = writeMetricsIfDue(connected, lastMetricsAt)
                        }
                    }
                }
            } catch (error: Exception) {
                val lostRecoveryReadyTransport = recoveryReady.getAndSet(false)
                controlReady.set(false)
                if (running.get() && !paused.get()) {
                    Log.w(TAG, "Control lane degraded", error)
                    listener.onControlDegraded(error)
                    if (lostRecoveryReadyTransport) {
                        listener.onControlTransportLost(error)
                    }
                    val retryDelayMs =
                        if (wakePending.get()) WAKE_RECONNECT_DELAY_MS else delayMs
                    waitForReconnect(retryDelayMs)
                    delayMs = nextControlReconnectDelayMs(retryDelayMs)
                }
            } finally {
                controlReady.set(false)
                recoveryReady.set(false)
                suspendAcknowledgement.getAndSet(null)?.countDown()
                synchronized(pauseLock) {
                    socket = null
                }
            }
        }
    }

    override fun close() {
        synchronized(pauseLock) {
            if (!running.compareAndSet(true, false)) return
            paused.set(false)
            controlReady.set(false)
            recoveryReady.set(false)
            suspendAcknowledgement.getAndSet(null)?.countDown()
            socket?.close()
            pauseLock.notifyAll()
        }
        thread?.interrupt()
        if (Thread.currentThread() !== thread) {
            thread?.join(2_000)
        }
        thread = null
    }

    private fun isActiveSocket(connected: Socket): Boolean =
        running.get() && !paused.get() && socket === connected && !connected.isClosed

    fun isRecoveryReady(): Boolean = recoveryReady.get()

    private fun awaitResume(): Boolean {
        synchronized(pauseLock) {
            while (running.get() && paused.get()) {
                try {
                    pauseLock.wait()
                } catch (_: InterruptedException) {
                    if (!running.get()) return false
                }
            }
        }
        return running.get()
    }

    private fun waitForReconnect(delayMs: Long) {
        synchronized(pauseLock) {
            if (!running.get() || paused.get()) return
            try {
                pauseLock.wait(delayMs)
            } catch (_: InterruptedException) {
                if (!running.get()) Thread.currentThread().interrupt()
            }
        }
    }

    private fun write(connected: Socket, frame: Gnr4Frame) {
        synchronized(writeLock) {
            Gnr4.write(connected.getOutputStream(), frame)
        }
    }

    private fun writeMetricsIfDue(connected: Socket, lastMetricsAt: Long): Long {
        val now = System.nanoTime()
        if (now - lastMetricsAt < METRICS_INTERVAL_NANOS) return lastMetricsAt
        val metrics = runCatching(metricsProvider).getOrNull() ?: return now
        write(
            connected,
            Gnr4Frame(Gnr4MessageType.METRICS, sessionId, Gnr4.metricsPayload(metrics)),
        )
        return now
    }

    companion object {
        private const val TAG = "Gnr4Control"
        private const val CONNECT_TIMEOUT_MS = 1_000
        private const val CONTROL_READ_TIMEOUT_MS = 1_000
        private const val WAKE_CONFIRM_TIMEOUT_MS = 250
        private const val RECONNECT_INITIAL_DELAY_MS = 250L
        private const val WAKE_RECONNECT_DELAY_MS = 100L
        private const val STOP_ACK_TIMEOUT_MS = 5_000L
        private const val SUSPEND_ACK_TIMEOUT_MS = 500L
        private const val MAX_OUTSTANDING_HEARTBEATS = 8
        private const val METRICS_INTERVAL_NANOS = 1_000_000_000L
        private val IPV4_LOOPBACK = java.net.InetAddress.getByAddress(byteArrayOf(127, 0, 0, 1))
    }
}
