package com.genymobile.gnirehtet.v4

import android.content.Context
import hev.htproxy.TProxyService
import java.io.File

internal interface NativeTunnel {
    fun start(tunFd: Int, socksPort: Int, udpPort: Int, mtu: Int = 1500)

    fun awaitReady(timeoutMs: Int)

    fun requestStop()

    fun awaitStopped(timeoutMs: Int): Boolean

    fun stats(): LongArray
}

internal interface HevNativeBridge {
    fun start(configPath: String, tunFd: Int): Long

    fun awaitReady(token: Long, timeoutMs: Int): Int

    fun stop(token: Long): Boolean

    fun awaitStopped(token: Long, timeoutMs: Int): Int

    fun stats(): LongArray
}

internal class HevProcessState {
    val lock = Any()
    var activeToken = 0L
}

private class JniHevNativeBridge : HevNativeBridge {
    private val service = TProxyService()

    override fun start(configPath: String, tunFd: Int): Long =
        service.TProxyStartService(configPath, tunFd)

    override fun awaitReady(token: Long, timeoutMs: Int): Int =
        service.TProxyAwaitReady(token, timeoutMs)

    override fun stop(token: Long): Boolean = service.TProxyStopService(token)

    override fun awaitStopped(token: Long, timeoutMs: Int): Int =
        service.TProxyAwaitStopped(token, timeoutMs)

    override fun stats(): LongArray = service.TProxyGetStats()
}

internal class HevTunnel(
    private val configDirectory: File,
    private val bridge: HevNativeBridge,
    private val processState: HevProcessState = HevProcessState(),
) : NativeTunnel {
    constructor(context: Context) : this(context.cacheDir, JniHevNativeBridge(), sharedProcessState)

    private var token = 0L

    override fun start(tunFd: Int, socksPort: Int, udpPort: Int, mtu: Int) {
        val config = File(configDirectory, "hev-vd.yml")
        config.writeText(renderHevConfig(mtu, socksPort, udpPort))
        synchronized(processState.lock) {
            check(token == 0L) { "HEV tunnel was started twice" }
            val staleToken = processState.activeToken
            if (staleToken != 0L) {
                check(bridge.stop(staleToken)) { "HEV rejected the orphaned generation's stop request" }
                check(isTerminalStop(bridge.awaitStopped(staleToken, ORPHAN_STOP_TIMEOUT_MS))) {
                    "HEV orphaned generation did not stop within ${ORPHAN_STOP_TIMEOUT_MS}ms"
                }
                processState.activeToken = 0L
            }
            val startedToken = bridge.start(config.absolutePath, tunFd)
            check(startedToken != 0L) { "HEV native engine could not start" }
            token = startedToken
            processState.activeToken = startedToken
        }
    }

    override fun awaitReady(timeoutMs: Int) {
        require(timeoutMs >= 0) { "timeoutMs must not be negative" }
        val activeToken = currentToken()
        when (bridge.awaitReady(activeToken, timeoutMs)) {
            1 -> Unit
            0 -> throw IllegalStateException("HEV initialization timed out after ${timeoutMs}ms")
            -2 -> throw IllegalStateException("HEV initialization belongs to a stale generation")
            else -> throw IllegalStateException("HEV initialization failed")
        }
    }

    override fun requestStop() {
        synchronized(processState.lock) {
            if (token == 0L) return
            if (processState.activeToken != token) {
                token = 0L
                return
            }
            check(bridge.stop(token)) { "HEV rejected the active generation's stop request" }
        }
    }

    override fun awaitStopped(timeoutMs: Int): Boolean {
        require(timeoutMs >= 0) { "timeoutMs must not be negative" }
        return synchronized(processState.lock) {
            if (token == 0L) return@synchronized true
            if (processState.activeToken != token) {
                token = 0L
                return@synchronized true
            }
            val activeToken = token
            val terminated = isTerminalStop(bridge.awaitStopped(activeToken, timeoutMs))
            if (terminated) {
                token = 0L
                if (processState.activeToken == activeToken) processState.activeToken = 0L
            }
            terminated
        }
    }

    override fun stats(): LongArray = synchronized(processState.lock) {
        if (token == 0L) LongArray(4) else bridge.stats().copyOf()
    }

    private fun currentToken(): Long = synchronized(processState.lock) {
        check(token != 0L) { "HEV tunnel has not started" }
        token
    }

    private fun isTerminalStop(result: Int): Boolean = result == 1 || result == -1 || result == -2

    companion object {
        private val sharedProcessState = HevProcessState()
        private const val ORPHAN_STOP_TIMEOUT_MS = 1_500
    }
}

internal fun renderHevConfig(mtu: Int, socksPort: Int, udpPort: Int): String {
    require(mtu in 576..65_535) { "mtu is outside 576..65535" }
    require(socksPort in 1_024..65_535) { "socksPort is outside 1024..65535" }
    require(udpPort in 1_024..65_535) { "udpPort is outside 1024..65535" }
    require(socksPort != udpPort) { "socksPort and udpPort must differ" }
    return """
        tunnel:
          mtu: $mtu
        socks5:
          address: '127.0.0.1'
          port: $socksPort
          udp-port: $udpPort
          udp: 'tcp'
        misc:
          task-stack-size: 65536
          tcp-buffer-size: 65536
          udp-recv-buffer-size: 524288
          udp-copy-buffer-nums: 32
          max-session-count: 256
          connect-timeout: 5000
          tcp-read-write-timeout: 0
          udp-read-write-timeout: 120000
          log-level: warn
        """.trimIndent()
}
