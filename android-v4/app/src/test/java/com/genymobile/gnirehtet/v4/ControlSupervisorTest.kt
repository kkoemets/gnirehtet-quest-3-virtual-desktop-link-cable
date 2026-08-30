package com.genymobile.gnirehtet.v4

import java.net.InetAddress
import java.net.InetSocketAddress
import java.net.ServerSocket
import java.net.SocketTimeoutException
import java.nio.charset.StandardCharsets
import java.util.UUID
import java.util.concurrent.CountDownLatch
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class ControlSupervisorTest {
    @Test
    fun reconnectBackoffStaysInsideLifecycleGate() {
        assertEquals(500L, nextControlReconnectDelayMs(250L))
        assertEquals(1_000L, nextControlReconnectDelayMs(500L))
        assertEquals(1_000L, nextControlReconnectDelayMs(1_000L))
        assertEquals(1_000L, nextControlReconnectDelayMs(Long.MAX_VALUE))
    }

    @Test
    fun initialConnectionFailureDoesNotReportTransportLoss() {
        val unavailable = ServerSocket().apply {
            bind(InetSocketAddress(InetAddress.getByName("127.0.0.1"), 0))
        }
        val port = unavailable.localPort
        unavailable.close()
        val degraded = CountDownLatch(1)
        val transportLosses = AtomicInteger()
        val supervisor = ControlSupervisor(
            UUID.randomUUID(),
            port,
            object : ControlSupervisor.Listener {
                override fun shouldReportStarted(): Boolean = true
                override fun onControlConnected() = Unit
                override fun onControlDegraded(error: Exception?) = degraded.countDown()
                override fun onControlTransportLost(error: Exception?) {
                    transportLosses.incrementAndGet()
                }
                override fun onControlRttSample(rttNanos: Long) = Unit
                override fun onControlStopRequested(sendStopped: () -> Unit) = Unit
            },
        )

        supervisor.start()
        assertTrue(degraded.await(2, TimeUnit.SECONDS))
        supervisor.close()
        assertEquals(0, transportLosses.get())
    }

    @Test
    fun authenticatedConnectionReportsOneTransportLoss() {
        val session = UUID.fromString("10203040-5060-7080-90a0-b0c0d0e0f000")
        val server = ServerSocket().apply {
            bind(InetSocketAddress(InetAddress.getByName("127.0.0.1"), 0))
        }
        val executor = Executors.newSingleThreadExecutor()
        val exchange = executor.submit {
            server.use {
                it.accept().use { connection ->
                    assertEquals(Gnr4MessageType.HELLO, Gnr4.read(connection.getInputStream(), session).type)
                    Gnr4.write(
                        connection.getOutputStream(),
                        Gnr4Frame(Gnr4MessageType.HELLO_ACK, session),
                    )
                    assertEquals(Gnr4MessageType.STARTED, Gnr4.read(connection.getInputStream(), session).type)
                }
            }
        }
        val connected = CountDownLatch(1)
        val transportLost = CountDownLatch(1)
        val transportLosses = AtomicInteger()
        val supervisor = ControlSupervisor(
            session,
            server.localPort,
            object : ControlSupervisor.Listener {
                override fun shouldReportStarted(): Boolean = true
                override fun onControlConnected() = connected.countDown()
                override fun onControlDegraded(error: Exception?) = Unit
                override fun onControlTransportLost(error: Exception?) {
                    transportLosses.incrementAndGet()
                    transportLost.countDown()
                }
                override fun onControlRttSample(rttNanos: Long) = Unit
                override fun onControlStopRequested(sendStopped: () -> Unit) = Unit
            },
        )

        supervisor.start()
        assertTrue(connected.await(2, TimeUnit.SECONDS))
        exchange.get(2, TimeUnit.SECONDS)
        assertTrue(transportLost.await(2, TimeUnit.SECONDS))
        supervisor.close()
        executor.shutdownNow()
        assertEquals(1, transportLosses.get())
    }

    @Test
    fun usesIpv4LoopbackAndAcknowledgesStop() {
        val session = UUID.fromString("00112233-4455-6677-8899-aabbccddeeff")
        val server = ServerSocket().apply {
            bind(InetSocketAddress(InetAddress.getByName("127.0.0.1"), 0))
        }
        val executor = Executors.newSingleThreadExecutor()
        val exchange = executor.submit {
            server.use {
                it.accept().use { connection ->
                    assertEquals("127.0.0.1", connection.inetAddress.hostAddress)
                    val hello = Gnr4.read(connection.getInputStream(), session)
                    assertEquals(Gnr4MessageType.HELLO, hello.type)
                    assertEquals(Gnr4.HELLO_CAPABILITIES, String(hello.payload, StandardCharsets.UTF_8))
                    Gnr4.write(
                        connection.getOutputStream(),
                        Gnr4Frame(Gnr4MessageType.HELLO_ACK, session),
                    )
                    assertEquals(Gnr4MessageType.STARTED, Gnr4.read(connection.getInputStream(), session).type)
                    Gnr4.write(connection.getOutputStream(), Gnr4Frame(Gnr4MessageType.STOP, session))
                    assertEquals(Gnr4MessageType.STOPPED, Gnr4.read(connection.getInputStream(), session).type)
                }
            }
        }
        val connected = CountDownLatch(1)
        val supervisor = ControlSupervisor(
            session,
            server.localPort,
            object : ControlSupervisor.Listener {
                override fun shouldReportStarted(): Boolean = true

                override fun onControlConnected() {
                    connected.countDown()
                }

                override fun onControlDegraded(error: Exception?) = Unit

                override fun onControlRttSample(rttNanos: Long) = Unit

                override fun onControlStopRequested(sendStopped: () -> Unit) = sendStopped()
            },
        )

        supervisor.start()
        assertTrue(connected.await(2, TimeUnit.SECONDS))
        exchange.get(2, TimeUnit.SECONDS)
        supervisor.close()
        executor.shutdownNow()
    }

    @Test
    fun legacyAndMalformedHelloAcknowledgementsDisableMetricsWithoutDisconnecting() {
        listOf(
            ByteArray(0),
            """{"capabilities":["heartbeat","status","explicit_stop","explicit_suspend"],"protocol":4}"""
                .toByteArray(StandardCharsets.UTF_8),
            """{"capabilities":["metrics_v1"]""".toByteArray(StandardCharsets.UTF_8),
        ).forEachIndexed { index, acknowledgementPayload ->
            val session = UUID(0x5061728394a5b6c7L, 0x8899aabbccddeeffUL.toLong() + index)
            val expected = Gnr4Metrics(11, 12, 13, 14, 15, 16, 17)
            val server = ServerSocket().apply {
                bind(InetSocketAddress(InetAddress.getByName("127.0.0.1"), 0))
            }
            val executor = Executors.newSingleThreadExecutor()
            val exchange = executor.submit {
                server.use {
                    it.accept().use { connection ->
                        connection.soTimeout = 2_500
                        assertEquals(Gnr4MessageType.HELLO, Gnr4.read(connection.getInputStream(), session).type)
                        Gnr4.write(
                            connection.getOutputStream(),
                            Gnr4Frame(Gnr4MessageType.HELLO_ACK, session, acknowledgementPayload),
                        )
                        assertEquals(Gnr4MessageType.STARTED, Gnr4.read(connection.getInputStream(), session).type)
                        val heartbeat = Gnr4.read(connection.getInputStream(), session)
                        assertEquals(Gnr4MessageType.HEARTBEAT, heartbeat.type)
                        Gnr4.write(connection.getOutputStream(), heartbeat)
                        connection.soTimeout = 400
                        try {
                            val unexpected = Gnr4.read(connection.getInputStream(), session)
                            throw AssertionError("legacy HELLO_ACK enabled ${unexpected.type}")
                        } catch (_: SocketTimeoutException) {
                            Unit
                        }
                        connection.soTimeout = 2_000
                        Gnr4.write(connection.getOutputStream(), Gnr4Frame(Gnr4MessageType.STOP, session))
                        assertEquals(Gnr4MessageType.STOPPED, Gnr4.read(connection.getInputStream(), session).type)
                    }
                }
            }
            val connected = CountDownLatch(1)
            val supervisor = ControlSupervisor(
                session,
                server.localPort,
                object : ControlSupervisor.Listener {
                    override fun shouldReportStarted(): Boolean = true
                    override fun onControlConnected() = connected.countDown()
                    override fun onControlDegraded(error: Exception?) = Unit
                    override fun onControlRttSample(rttNanos: Long) = Unit
                    override fun onControlStopRequested(sendStopped: () -> Unit) = sendStopped()
                },
                metricsProvider = { expected },
            )

            supervisor.start()
            try {
                assertTrue(connected.await(2, TimeUnit.SECONDS))
                exchange.get(5, TimeUnit.SECONDS)
            } finally {
                supervisor.close()
                executor.shutdownNow()
            }
        }
    }

    @Test
    fun sendsBoundedMetricsOnceTheControlLaneIsActive() {
        val session = UUID.fromString("40516273-8495-a6b7-c8d9-eafb0c1d2e3f")
        val expected = Gnr4Metrics(11, 12, 13, 14, 15, 16, 17)
        val server = ServerSocket().apply {
            bind(InetSocketAddress(InetAddress.getByName("127.0.0.1"), 0))
        }
        val executor = Executors.newSingleThreadExecutor()
        val exchange = executor.submit {
            server.use {
                it.accept().use { connection ->
                    connection.soTimeout = 3_500
                    assertEquals(Gnr4MessageType.HELLO, Gnr4.read(connection.getInputStream(), session).type)
                    Gnr4.write(
                        connection.getOutputStream(),
                        Gnr4Frame(
                            Gnr4MessageType.HELLO_ACK,
                            session,
                            """{"protocol":4,"capabilities":["heartbeat","metrics_v1"]}"""
                                .toByteArray(StandardCharsets.UTF_8),
                        ),
                    )
                    assertEquals(Gnr4MessageType.STARTED, Gnr4.read(connection.getInputStream(), session).type)
                    var frame: Gnr4Frame
                    do {
                        frame = Gnr4.read(connection.getInputStream(), session)
                    } while (frame.type != Gnr4MessageType.METRICS)
                    assertEquals(expected, Gnr4.parseMetricsPayload(frame.payload))
                    Gnr4.write(connection.getOutputStream(), Gnr4Frame(Gnr4MessageType.STOP, session))
                    assertEquals(Gnr4MessageType.STOPPED, Gnr4.read(connection.getInputStream(), session).type)
                }
            }
        }
        val connected = CountDownLatch(1)
        val supervisor = ControlSupervisor(
            session,
            server.localPort,
            object : ControlSupervisor.Listener {
                override fun shouldReportStarted(): Boolean = true
                override fun onControlConnected() = connected.countDown()
                override fun onControlDegraded(error: Exception?) = Unit
                override fun onControlRttSample(rttNanos: Long) = Unit
                override fun onControlStopRequested(sendStopped: () -> Unit) = sendStopped()
            },
            metricsProvider = { expected },
        )

        supervisor.start()
        assertTrue(connected.await(2, TimeUnit.SECONDS))
        exchange.get(4, TimeUnit.SECONDS)
        supervisor.close()
        executor.shutdownNow()
    }

    @Test
    fun suspendedSupervisorWaitsForWakeBeforeConnecting() {
        val session = UUID.fromString("10213243-5465-7687-98a9-bacbdcedfe0f")
        val server = ServerSocket().apply {
            bind(InetSocketAddress(InetAddress.getByName("127.0.0.1"), 0))
        }
        val executor = Executors.newSingleThreadExecutor()
        val exchange = executor.submit {
            server.use {
                it.accept().use { connection ->
                    assertEquals(Gnr4MessageType.HELLO, Gnr4.read(connection.getInputStream(), session).type)
                    Gnr4.write(
                        connection.getOutputStream(),
                        Gnr4Frame(Gnr4MessageType.HELLO_ACK, session),
                    )
                    val started = Gnr4.read(connection.getInputStream(), session)
                    assertEquals(Gnr4MessageType.STARTED, started.type)
                    assertTrue(started.payload.contentEquals(Gnr4.startedPayload(wake = true)))
                    Gnr4.write(
                        connection.getOutputStream(),
                        Gnr4Frame(Gnr4MessageType.STATUS, session),
                    )
                }
            }
        }
        val connected = CountDownLatch(1)
        val supervisor = ControlSupervisor(
            session,
            server.localPort,
            object : ControlSupervisor.Listener {
                override fun shouldReportStarted(): Boolean = true
                override fun onControlConnected() = connected.countDown()
                override fun onControlDegraded(error: Exception?) = Unit
                override fun onControlRttSample(rttNanos: Long) = Unit
                override fun onControlStopRequested(sendStopped: () -> Unit) = Unit
            },
        )

        supervisor.start(startPaused = true)
        assertFalse(connected.await(150, TimeUnit.MILLISECONDS))
        supervisor.resume()
        assertTrue(connected.await(2, TimeUnit.SECONDS))
        exchange.get(2, TimeUnit.SECONDS)
        supervisor.close()
        executor.shutdownNow()
    }

    @Test
    fun acknowledgedSuspendResumesOnTheSameControlConnection() {
        val session = UUID.fromString("20314253-6475-8697-a8b9-cadbecfd0e1f")
        val server = ServerSocket().apply {
            bind(InetSocketAddress(InetAddress.getByName("127.0.0.1"), 0))
        }
        val executor = Executors.newSingleThreadExecutor()
        val exchange = executor.submit {
            server.use {
                it.accept().use { connection ->
                    assertEquals(Gnr4MessageType.HELLO, Gnr4.read(connection.getInputStream(), session).type)
                    Gnr4.write(
                        connection.getOutputStream(),
                        Gnr4Frame(Gnr4MessageType.HELLO_ACK, session),
                    )
                    assertEquals(Gnr4MessageType.STARTED, Gnr4.read(connection.getInputStream(), session).type)
                    assertEquals(Gnr4MessageType.SUSPEND, Gnr4.read(connection.getInputStream(), session).type)
                    Gnr4.write(
                        connection.getOutputStream(),
                        Gnr4Frame(Gnr4MessageType.SUSPENDED, session),
                    )
                    val resumed = Gnr4.read(connection.getInputStream(), session)
                    assertEquals(Gnr4MessageType.STARTED, resumed.type)
                    assertTrue(resumed.payload.contentEquals(Gnr4.startedPayload(wake = true)))
                    Gnr4.write(
                        connection.getOutputStream(),
                        Gnr4Frame(Gnr4MessageType.STATUS, session),
                    )
                    Gnr4.write(connection.getOutputStream(), Gnr4Frame(Gnr4MessageType.STOP, session))
                    assertEquals(Gnr4MessageType.STOPPED, Gnr4.read(connection.getInputStream(), session).type)
                }
            }
        }
        val initialConnected = CountDownLatch(1)
        val resumedConnected = CountDownLatch(1)
        val connectedCallbacks = AtomicInteger()
        val supervisor = ControlSupervisor(
            session,
            server.localPort,
            object : ControlSupervisor.Listener {
                override fun shouldReportStarted(): Boolean = true
                override fun onControlConnected() {
                    if (connectedCallbacks.incrementAndGet() == 1) {
                        initialConnected.countDown()
                    } else {
                        resumedConnected.countDown()
                    }
                }
                override fun onControlDegraded(error: Exception?) = Unit
                override fun onControlRttSample(rttNanos: Long) = Unit
                override fun onControlStopRequested(sendStopped: () -> Unit) = sendStopped()
            },
        )

        supervisor.start()
        assertTrue(initialConnected.await(2, TimeUnit.SECONDS))
        supervisor.suspend()
        Thread.sleep(1_200)
        supervisor.resume()
        assertTrue(resumedConnected.await(2, TimeUnit.SECONDS))
        exchange.get(2, TimeUnit.SECONDS)
        supervisor.close()
        executor.shutdownNow()
    }

    @Test
    fun failedReusedWakeRepeatsTheWakeMarkerOnTheReplacementConnection() {
        val session = UUID.fromString("2a415263-7485-96a7-b8c9-daebfc0d1e2f")
        val server = ServerSocket().apply {
            bind(InetSocketAddress(InetAddress.getByName("127.0.0.1"), 0))
        }
        val executor = Executors.newSingleThreadExecutor()
        val exchange = executor.submit {
            server.use {
                it.accept().use { first ->
                    assertEquals(Gnr4MessageType.HELLO, Gnr4.read(first.getInputStream(), session).type)
                    Gnr4.write(first.getOutputStream(), Gnr4Frame(Gnr4MessageType.HELLO_ACK, session))
                    assertEquals(Gnr4MessageType.STARTED, Gnr4.read(first.getInputStream(), session).type)
                    assertEquals(Gnr4MessageType.SUSPEND, Gnr4.read(first.getInputStream(), session).type)
                    Gnr4.write(first.getOutputStream(), Gnr4Frame(Gnr4MessageType.SUSPENDED, session))
                    val attemptedWake = Gnr4.read(first.getInputStream(), session)
                    assertEquals(Gnr4MessageType.STARTED, attemptedWake.type)
                    assertTrue(attemptedWake.payload.contentEquals(Gnr4.startedPayload(wake = true)))
                }
                it.accept().use { replacement ->
                    assertEquals(
                        Gnr4MessageType.HELLO,
                        Gnr4.read(replacement.getInputStream(), session).type,
                    )
                    Gnr4.write(
                        replacement.getOutputStream(),
                        Gnr4Frame(Gnr4MessageType.HELLO_ACK, session),
                    )
                    val replacementWake = Gnr4.read(replacement.getInputStream(), session)
                    assertEquals(Gnr4MessageType.STARTED, replacementWake.type)
                    assertTrue(replacementWake.payload.contentEquals(Gnr4.startedPayload(wake = true)))
                    Gnr4.write(
                        replacement.getOutputStream(),
                        Gnr4Frame(Gnr4MessageType.STATUS, session),
                    )
                    Gnr4.write(
                        replacement.getOutputStream(),
                        Gnr4Frame(Gnr4MessageType.STOP, session),
                    )
                    assertEquals(
                        Gnr4MessageType.STOPPED,
                        Gnr4.read(replacement.getInputStream(), session).type,
                    )
                }
            }
        }
        val initialConnected = CountDownLatch(1)
        val supervisor = ControlSupervisor(
            session,
            server.localPort,
            object : ControlSupervisor.Listener {
                override fun shouldReportStarted(): Boolean = true
                override fun onControlConnected() = initialConnected.countDown()
                override fun onControlDegraded(error: Exception?) = Unit
                override fun onControlRttSample(rttNanos: Long) = Unit
                override fun onControlStopRequested(sendStopped: () -> Unit) = sendStopped()
            },
        )

        supervisor.start()
        assertTrue(initialConnected.await(2, TimeUnit.SECONDS))
        supervisor.suspend()
        supervisor.resume()
        exchange.get(4, TimeUnit.SECONDS)
        supervisor.close()
        executor.shutdownNow()
    }

    @Test
    fun halfOpenWakeReconnectsAndAuthenticatesWithinOneSecond() {
        val session = UUID.fromString("3a516273-8495-a6b7-c8d9-eafb0c1d2e3f")
        val server = ServerSocket().apply {
            bind(InetSocketAddress(InetAddress.getByName("127.0.0.1"), 0))
        }
        val executor = Executors.newSingleThreadExecutor()
        val exchange = executor.submit {
            server.use {
                val first = it.accept()
                first.use { connection ->
                    assertEquals(
                        Gnr4MessageType.HELLO,
                        Gnr4.read(connection.getInputStream(), session).type,
                    )
                    Gnr4.write(
                        connection.getOutputStream(),
                        Gnr4Frame(Gnr4MessageType.HELLO_ACK, session),
                    )
                    assertEquals(
                        Gnr4MessageType.STARTED,
                        Gnr4.read(connection.getInputStream(), session).type,
                    )
                    assertEquals(
                        Gnr4MessageType.SUSPEND,
                        Gnr4.read(connection.getInputStream(), session).type,
                    )
                    Gnr4.write(
                        connection.getOutputStream(),
                        Gnr4Frame(Gnr4MessageType.SUSPENDED, session),
                    )
                    val attemptedWake = Gnr4.read(connection.getInputStream(), session)
                    assertEquals(Gnr4MessageType.STARTED, attemptedWake.type)
                    assertTrue(attemptedWake.payload.contentEquals(Gnr4.startedPayload(wake = true)))

                    val reconnectStarted = System.nanoTime()
                    it.accept().use { replacement ->
                        val reconnectMs =
                            TimeUnit.NANOSECONDS.toMillis(System.nanoTime() - reconnectStarted)
                        assertTrue("wake replacement took ${reconnectMs}ms", reconnectMs < 1_000)
                        assertEquals(
                            Gnr4MessageType.HELLO,
                            Gnr4.read(replacement.getInputStream(), session).type,
                        )
                        Gnr4.write(
                            replacement.getOutputStream(),
                            Gnr4Frame(Gnr4MessageType.HELLO_ACK, session),
                        )
                        val replacementWake = Gnr4.read(replacement.getInputStream(), session)
                        assertEquals(Gnr4MessageType.STARTED, replacementWake.type)
                        assertTrue(
                            replacementWake.payload.contentEquals(Gnr4.startedPayload(wake = true)),
                        )
                        Gnr4.write(
                            replacement.getOutputStream(),
                            Gnr4Frame(Gnr4MessageType.STATUS, session),
                        )
                        Gnr4.write(
                            replacement.getOutputStream(),
                            Gnr4Frame(Gnr4MessageType.STOP, session),
                        )
                        assertEquals(
                            Gnr4MessageType.STOPPED,
                            Gnr4.read(replacement.getInputStream(), session).type,
                        )
                    }
                }
            }
        }
        val initialConnected = CountDownLatch(1)
        val resumedConnected = CountDownLatch(1)
        val connectedCallbacks = AtomicInteger()
        val supervisor = ControlSupervisor(
            session,
            server.localPort,
            object : ControlSupervisor.Listener {
                override fun shouldReportStarted(): Boolean = true
                override fun onControlConnected() {
                    if (connectedCallbacks.incrementAndGet() == 1) {
                        initialConnected.countDown()
                    } else {
                        resumedConnected.countDown()
                    }
                }
                override fun onControlDegraded(error: Exception?) = Unit
                override fun onControlRttSample(rttNanos: Long) = Unit
                override fun onControlStopRequested(sendStopped: () -> Unit) = sendStopped()
            },
        )

        supervisor.start()
        assertTrue(initialConnected.await(2, TimeUnit.SECONDS))
        supervisor.suspend()
        supervisor.resume()
        assertTrue(resumedConnected.await(1, TimeUnit.SECONDS))
        exchange.get(3, TimeUnit.SECONDS)
        supervisor.close()
        executor.shutdownNow()
    }

    @Test
    fun missingSuspendAcknowledgementStillClosesWithinTheBound() {
        val session = UUID.fromString("30415263-7485-96a7-b8c9-daebfc0d1e2f")
        val server = ServerSocket().apply {
            bind(InetSocketAddress(InetAddress.getByName("127.0.0.1"), 0))
        }
        val executor = Executors.newSingleThreadExecutor()
        val exchange = executor.submit {
            server.use {
                it.accept().use { connection ->
                    assertEquals(Gnr4MessageType.HELLO, Gnr4.read(connection.getInputStream(), session).type)
                    Gnr4.write(
                        connection.getOutputStream(),
                        Gnr4Frame(Gnr4MessageType.HELLO_ACK, session),
                    )
                    assertEquals(Gnr4MessageType.STARTED, Gnr4.read(connection.getInputStream(), session).type)
                    assertEquals(Gnr4MessageType.SUSPEND, Gnr4.read(connection.getInputStream(), session).type)
                    assertEquals(-1, connection.getInputStream().read())
                }
            }
        }
        val connected = CountDownLatch(1)
        val supervisor = ControlSupervisor(
            session,
            server.localPort,
            object : ControlSupervisor.Listener {
                override fun shouldReportStarted(): Boolean = true
                override fun onControlConnected() = connected.countDown()
                override fun onControlDegraded(error: Exception?) = Unit
                override fun onControlRttSample(rttNanos: Long) = Unit
                override fun onControlStopRequested(sendStopped: () -> Unit) = Unit
            },
        )

        supervisor.start()
        assertTrue(connected.await(2, TimeUnit.SECONDS))
        val started = System.nanoTime()
        supervisor.suspend()
        val elapsedMs = TimeUnit.NANOSECONDS.toMillis(System.nanoTime() - started)
        assertTrue("suspend exceeded its bounded wait: ${elapsedMs}ms", elapsedMs < 1_000)
        exchange.get(2, TimeUnit.SECONDS)
        supervisor.close()
        executor.shutdownNow()
    }
}
