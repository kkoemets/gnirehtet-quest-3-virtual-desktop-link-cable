package com.genymobile.gnirehtet.v4

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.ApplicationInfo
import android.content.pm.PackageManager
import android.content.pm.ServiceInfo
import android.hardware.display.DisplayManager
import android.net.VpnService
import android.os.Build
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.os.ParcelFileDescriptor
import android.os.PowerManager
import android.os.Process
import android.os.SystemClock
import android.util.Log
import android.view.Display
import java.io.FileDescriptor
import java.io.IOException
import java.io.PrintWriter
import java.util.UUID
import java.util.concurrent.CountDownLatch
import java.util.concurrent.ExecutorService
import java.util.concurrent.Executors
import java.util.concurrent.ThreadFactory
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicInteger
import java.util.concurrent.atomic.AtomicLong
import java.util.concurrent.atomic.AtomicReference

internal fun canFinishRejectedStart(hasActiveResources: Boolean, teardownInProgress: Boolean): Boolean =
    !hasActiveResources && !teardownInProgress

internal fun stopTargetsGeneration(
    expectedGeneration: Long?,
    activeGeneration: Long?,
    teardownInProgress: Boolean,
    closingGeneration: Long,
    expectedIsCurrent: Boolean,
): Boolean = expectedGeneration == null ||
    expectedIsCurrent ||
    activeGeneration == expectedGeneration ||
    (teardownInProgress && closingGeneration == expectedGeneration)

internal fun isHeadsetDisplaySuspended(displayState: Int?, isInteractive: Boolean): Boolean =
    !isInteractive || when (displayState) {
        Display.STATE_ON, Display.STATE_VR -> false
        Display.STATE_OFF,
        Display.STATE_DOZE,
        Display.STATE_DOZE_SUSPEND,
        Display.STATE_ON_SUSPEND,
        -> true
        else -> false
    }

internal fun screenSuspendedFromBroadcast(action: String?): Boolean? = when (action) {
    Intent.ACTION_SCREEN_OFF -> true
    Intent.ACTION_SCREEN_ON -> false
    else -> null
}

internal fun shouldDeferDisplaySuspension(
    suspended: Boolean,
    nowElapsedRealtimeMs: Long,
    wakeDebounceUntilMs: Long,
): Boolean = suspended && nowElapsedRealtimeMs < wakeDebounceUntilMs

internal class TransportEngineReset {
    private val pending = AtomicReference<CountDownLatch?>()

    fun require(): Boolean = pending.compareAndSet(null, CountDownLatch(1))

    fun isRequired(): Boolean = pending.get() != null

    fun awaitReady(timeoutMs: Long): Boolean {
        val completion = pending.get() ?: return true
        return try {
            completion.await(timeoutMs, TimeUnit.MILLISECONDS)
        } catch (_: InterruptedException) {
            Thread.currentThread().interrupt()
            false
        }
    }

    fun complete(): Boolean {
        val completion = pending.get() ?: return false
        if (!pending.compareAndSet(completion, null)) return false
        completion.countDown()
        return true
    }
}

class VdLinkVpnService : VpnService() {
    private data class SessionResources(
        val generation: Long,
        val parameters: SessionParameters,
        val vpnInterface: ParcelFileDescriptor,
        val tunnel: NativeTunnel,
        val engineStartLock: Any = Any(),
        val engineSuspended: AtomicBoolean = AtomicBoolean(),
        val engineResumeQueued: AtomicBoolean = AtomicBoolean(),
        val transportEngineReset: TransportEngineReset = TransportEngineReset(),
        @Volatile var control: ControlSupervisor? = null,
    )

    private val lifecycleLock = Any()
    private val generationGate = GenerationGate()
    private val stopAcknowledgements = StopAcknowledgements()
    private val mainHandler = Handler(Looper.getMainLooper())
    // Native startup may be waiting for readiness while an explicit Stop must
    // still tear the descriptor down promptly. Two workers cover that overlap
    // without allowing lifecycle work to grow an unbounded thread pool.
    private val worker: ExecutorService = Executors.newFixedThreadPool(2, LifecycleThreadFactory())
    private val destroyed = AtomicBoolean()
    private val screenSuspended = AtomicBoolean()
    private val screenReceiverRegistered = AtomicBoolean()
    private val displayListenerRegistered = AtomicBoolean()
    private val wakeDebounceUntilMs = AtomicLong()
    private val sleepStatePoller = object : Runnable {
        override fun run() {
            if (destroyed.get()) return
            refreshScreenState()
            mainHandler.postDelayed(this, SLEEP_STATE_POLL_INTERVAL_MS)
        }
    }

    private val screenReceiver = object : BroadcastReceiver() {
        override fun onReceive(context: Context?, intent: Intent?) {
            val action = intent?.action
            when (action) {
                Intent.ACTION_SCREEN_ON -> wakeDebounceUntilMs.set(
                    SystemClock.elapsedRealtime() + WAKE_DISPLAY_DEBOUNCE_MS,
                )
                Intent.ACTION_SCREEN_OFF -> wakeDebounceUntilMs.set(0)
            }
            screenSuspendedFromBroadcast(action)?.let(::setScreenSuspended)
        }
    }

    private val displayListener = object : DisplayManager.DisplayListener {
        override fun onDisplayAdded(displayId: Int) {
            if (displayId == Display.DEFAULT_DISPLAY) refreshScreenState()
        }

        override fun onDisplayRemoved(displayId: Int) {
            if (displayId == Display.DEFAULT_DISPLAY) refreshScreenState()
        }

        override fun onDisplayChanged(displayId: Int) {
            if (displayId == Display.DEFAULT_DISPLAY) refreshScreenState()
        }
    }

    private var active: SessionResources? = null
    private var teardownInProgress = false
    private var closingGeneration = 0L
    private var terminalAfterTeardown = LifecycleState.STOPPED
    @Volatile private var sessionId: UUID? = null

    override fun onCreate() {
        super.onCreate()
        getSystemService(DisplayManager::class.java).registerDisplayListener(displayListener, mainHandler)
        displayListenerRegistered.set(true)
        val filter = IntentFilter().apply {
            addAction(Intent.ACTION_SCREEN_OFF)
            addAction(Intent.ACTION_SCREEN_ON)
        }
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            registerReceiver(screenReceiver, filter, Context.RECEIVER_NOT_EXPORTED)
        } else {
            @Suppress("DEPRECATION")
            registerReceiver(screenReceiver, filter)
        }
        screenReceiverRegistered.set(true)
        refreshScreenState()
        mainHandler.postDelayed(sleepStatePoller, SLEEP_STATE_POLL_INTERVAL_MS)
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        when (intent?.action) {
            ACTION_STOP -> requestStop()
            ACTION_START -> startVpn(intent)
        }
        return Service.START_NOT_STICKY
    }

    private fun startVpn(intent: Intent) {
        val parameters = try {
            SessionParameters.parse(
                intent.getStringExtra(EXTRA_SESSION_ID),
                intent.getStringExtra(EXTRA_VD_PACKAGE),
                intent.getIntExtra(EXTRA_SOCKS_PORT, DEFAULT_SOCKS_PORT),
                intent.getIntExtra(EXTRA_UDP_PORT, DEFAULT_UDP_PORT),
                intent.getIntExtra(EXTRA_CONTROL_PORT, DEFAULT_CONTROL_PORT),
                intent.getBooleanExtra(EXTRA_ALL_TRAFFIC, DEFAULT_ALL_TRAFFIC),
            )
        } catch (error: RuntimeException) {
            rejectStart(error)
            return
        }

        val generation = synchronized(lifecycleLock) {
            val existing = active
            if (existing != null || teardownInProgress) {
                if (existing?.parameters?.sessionId != parameters.sessionId) {
                    lastError.set("Another wired-link session is already active")
                    updateNotification()
                }
                return
            }
            stopAcknowledgements.reset()
            generationGate.begin().also {
                state.set(LifecycleState.PREPARING)
                lastError.set(null)
                sessionId = parameters.sessionId
                controlRtt.reset()
            }
        }

        try {
            startForegroundCompat(buildNotification())
        } catch (error: RuntimeException) {
            failGeneration(generation, error)
            return
        }

        var established: ParcelFileDescriptor? = null
        try {
            val routingMode = VpnRoutingMode.from(parameters.allTraffic)
            if (routingMode.requiresVirtualDesktopPackage) {
                validateVdApplication(parameters.vdPackage)
            }
            val builder = Builder()
                .setSession(getString(R.string.app_name))
                .setMtu(MTU)
                .setMetered(false)
                .setUnderlyingNetworks(null)
                .addAddress("10.0.0.2", 32)
                .addRoute("0.0.0.0", 0)
                .addDnsServer("1.1.1.1")

            when (routingMode) {
                VpnRoutingMode.ALL_TRAFFIC -> builder.addDisallowedApplication(packageName)
                VpnRoutingMode.VIRTUAL_DESKTOP_ONLY ->
                    builder.addAllowedApplication(parameters.vdPackage)
            }

            established = builder.establish() ?: throw IllegalStateException("VPN permission is not prepared")
            val resources = SessionResources(
                generation,
                parameters,
                established,
                HevTunnel(this),
            )
            val accepted = synchronized(lifecycleLock) {
                if (generationGate.isCurrent(generation) && !teardownInProgress) {
                    active = resources
                    vpnDescriptorOpen.set(true)
                    true
                } else {
                    false
                }
            }
            if (!accepted) {
                established.close()
                return
            }
            established = null
            worker.execute { initializeNativeEngine(resources) }
        } catch (error: Throwable) {
            try {
                established?.close()
            } catch (closeError: IOException) {
                error.addSuppressed(closeError)
            }
            failGeneration(generation, error)
        }
    }

    private fun validateVdApplication(vdPackage: String) {
        val application = try {
            packageManager.getApplicationInfo(vdPackage, 0)
        } catch (error: PackageManager.NameNotFoundException) {
            throw IllegalStateException("Virtual Desktop is not installed: $vdPackage", error)
        }
        val identity = VdAppIdentity(
            application.packageName,
            application.uid,
            application.flags and (ApplicationInfo.FLAG_SYSTEM or ApplicationInfo.FLAG_UPDATED_SYSTEM_APP) != 0,
            packageManager.getPackagesForUid(application.uid)?.toSet().orEmpty(),
        )
        VdAppIdentityValidator.validate(identity, packageName, Process.myUid())
    }

    private fun initializeNativeEngine(resources: SessionResources) {
        try {
            synchronized(resources.engineStartLock) {
                if (!isCurrent(resources)) return
                resources.tunnel.start(
                    resources.vpnInterface.fd,
                    resources.parameters.socksPort,
                    resources.parameters.udpPort,
                    MTU,
                )
            }
            resources.tunnel.awaitReady(ENGINE_START_TIMEOUT_MS)
            if (!isCurrent(resources)) return

            lateinit var supervisor: ControlSupervisor
            supervisor = ControlSupervisor(
                resources.parameters.sessionId,
                resources.parameters.controlPort,
                object : ControlSupervisor.Listener {
                    override fun shouldReportStarted(): Boolean =
                        isCurrent(resources, supervisor) &&
                            !screenSuspended.get() &&
                            resources.transportEngineReset.awaitReady(ENGINE_START_TIMEOUT_MS.toLong()) &&
                            !resources.engineSuspended.get()

                    override fun onControlConnected() {
                        onControlReady(resources, supervisor)
                    }

                    override fun onControlDegraded(error: Exception?) {
                        if (isCurrent(resources, supervisor)) {
                            state.set(LifecycleState.DEGRADED)
                            lastError.set(error?.message)
                            updateNotification()
                        }
                    }

                    override fun onControlTransportLost(error: Exception?) {
                        onControlTransportLost(resources, supervisor)
                    }

                    override fun onControlRttSample(rttNanos: Long) {
                        if (isCurrent(resources, supervisor)) controlRtt.record(rttNanos)
                    }

                    override fun onControlStopRequested(sendStopped: () -> Unit) {
                        requestStop(resources.generation, sendStopped)
                    }
                },
                metricsProvider = {
                    if (isCurrent(resources, supervisor)) collectMetrics(resources.tunnel) else null
                },
            )
            val accepted = synchronized(lifecycleLock) {
                if (generationGate.isCurrent(resources.generation) && active === resources) {
                    resources.control = supervisor
                    // Start while holding the same lock used by screen-state
                    // delivery so a concurrent wake cannot be lost between
                    // publishing the supervisor and applying its initial pause.
                    supervisor.start(screenSuspended.get())
                    true
                } else {
                    false
                }
            }
            if (!accepted) {
                supervisor.close()
            } else if (screenSuspended.get()) {
                resources.engineSuspended.set(true)
                worker.execute { suspendNativeEngine(resources) }
            }
        } catch (error: Throwable) {
            failGeneration(resources.generation, error)
        }
    }

    private fun isCurrent(resources: SessionResources, supervisor: ControlSupervisor? = null): Boolean =
        synchronized(lifecycleLock) {
            generationGate.isCurrent(resources.generation) &&
                active === resources &&
                (supervisor == null || resources.control === supervisor)
        }

    private fun collectMetrics(tunnel: NativeTunnel): Gnr4Metrics? {
        val stats = runCatching(tunnel::stats).getOrNull() ?: return null
        if (
            stats.size < 4 || stats[0] < 0 || stats[1] < 0 ||
            stats[2] < 0 || stats[3] < 0
        ) return null
        val rtt = controlRtt.snapshot()
        return Gnr4Metrics(
            txPackets = stats[0],
            txBytes = stats[1],
            rxPackets = stats[2],
            rxBytes = stats[3],
            controlRttSamples = rtt.samples,
            controlRttP99Micros = rtt.p99Micros,
            controlRttMaxMicros = rtt.maxMicros,
        )
    }

    private fun setScreenSuspended(suspended: Boolean) {
        var resourcesToSuspend: SessionResources? = null
        synchronized(lifecycleLock) {
            if (!screenSuspended.compareAndSet(!suspended, suspended)) return
            val resources = active
            val control = resources?.control
            if (suspended) {
                resources?.engineSuspended?.set(true)
                control?.suspend()
                resourcesToSuspend = resources
                if (control != null && state.compareAndSet(LifecycleState.CONNECTED, LifecycleState.DEGRADED)) {
                    lastError.set("Headset is asleep; VPN remains ready to reconnect")
                    updateNotification()
                }
            } else if (resources?.engineSuspended?.get() == true) {
                queueNativeEngineResume(resources)
            } else {
                control?.resume()
            }
        }
        resourcesToSuspend?.let { worker.execute { suspendNativeEngine(it) } }
    }

    private fun onControlReady(resources: SessionResources, supervisor: ControlSupervisor) {
        if (
            !isCurrent(resources, supervisor) ||
            screenSuspended.get() ||
            !supervisor.isRecoveryReady()
        ) return
        if (resources.transportEngineReset.isRequired()) {
            resources.engineSuspended.set(true)
            queueNativeEngineResume(resources)
            return
        }
        state.set(LifecycleState.CONNECTED)
        lastError.set(null)
        updateNotification()
    }

    private fun onControlTransportLost(
        resources: SessionResources,
        supervisor: ControlSupervisor,
    ) {
        val resetEngine = synchronized(lifecycleLock) {
            if (
                !generationGate.isCurrent(resources.generation) ||
                active !== resources ||
                resources.control !== supervisor ||
                !resources.transportEngineReset.require()
            ) {
                false
            } else {
                resources.engineSuspended.set(true)
                !screenSuspended.get()
            }
        }
        if (resetEngine) queueNativeEngineResume(resources)
    }

    private fun suspendNativeEngine(resources: SessionResources) {
        synchronized(resources.engineStartLock) {
            if (!isCurrent(resources) || !resources.engineSuspended.get()) return
            runCatching { resources.tunnel.requestStop() }
                .onFailure { Log.w(TAG, "Could not stop the native engine for sleep", it) }
            runCatching { resources.tunnel.awaitStopped(ENGINE_STOP_TIMEOUT_MS) }
                .onFailure { Log.w(TAG, "Native engine sleep shutdown failed", it) }
        }
    }

    private fun queueNativeEngineResume(resources: SessionResources) {
        if (!resources.engineResumeQueued.compareAndSet(false, true)) return
        worker.execute {
            var retry = false
            var completedTransportReset = false
            var supervisor: ControlSupervisor? = null
            try {
                synchronized(resources.engineStartLock) {
                    if (
                        !isCurrent(resources) ||
                        screenSuspended.get() ||
                        !resources.engineSuspended.get()
                    ) return@execute
                    resources.tunnel.requestStop()
                    check(resources.tunnel.awaitStopped(ENGINE_STOP_TIMEOUT_MS)) {
                        "Native engine did not stop before wake restart"
                    }
                    resources.tunnel.start(
                        resources.vpnInterface.fd,
                        resources.parameters.socksPort,
                        resources.parameters.udpPort,
                        MTU,
                    )
                    resources.tunnel.awaitReady(ENGINE_START_TIMEOUT_MS)
                }
                synchronized(lifecycleLock) {
                    if (
                        !generationGate.isCurrent(resources.generation) ||
                        active !== resources ||
                        screenSuspended.get()
                    ) return@synchronized
                    resources.engineSuspended.set(false)
                    completedTransportReset = resources.transportEngineReset.complete()
                    supervisor = resources.control
                    resources.control?.resume()
                }
                if (completedTransportReset) {
                    supervisor?.let { onControlReady(resources, it) }
                }
            } catch (error: Throwable) {
                retry = isCurrent(resources) && !screenSuspended.get()
                Log.w(TAG, "Native engine wake restart failed", error)
                if (retry) {
                    state.set(LifecycleState.DEGRADED)
                    lastError.set(error.message ?: error.javaClass.simpleName)
                    updateNotification()
                }
            } finally {
                resources.engineResumeQueued.set(false)
                if (
                    (retry || resources.engineSuspended.get()) &&
                    isCurrent(resources) &&
                    !screenSuspended.get()
                ) {
                    mainHandler.postDelayed(
                        { queueNativeEngineResume(resources) },
                        ENGINE_RESTART_RETRY_MS,
                    )
                }
            }
        }
    }

    private fun refreshScreenState() {
        if (destroyed.get()) return
        val displayState = getSystemService(DisplayManager::class.java)
            .getDisplay(Display.DEFAULT_DISPLAY)
            ?.state
        val interactive = getSystemService(PowerManager::class.java).isInteractive
        val suspended = isHeadsetDisplaySuspended(displayState, interactive)
        if (
            shouldDeferDisplaySuspension(
                suspended,
                SystemClock.elapsedRealtime(),
                wakeDebounceUntilMs.get(),
            )
        ) return
        setScreenSuspended(suspended)
    }

    private fun failGeneration(generation: Long, error: Throwable) {
        synchronized(lifecycleLock) {
            if (!generationGate.isCurrent(generation)) return
        }
        Log.e(TAG, "v4 VPN session failed", error)
        lastError.set(error.message ?: error.javaClass.simpleName)
        requestStop(generation, failure = error)
    }

    private fun finishWithoutResources(error: Throwable) {
        Log.e(TAG, "Cannot prepare v4 VPN", error)
        lastError.set(error.message ?: error.javaClass.simpleName)
        state.set(LifecycleState.ERROR)
        vpnDescriptorOpen.set(false)
        mainHandler.post {
            stopForeground(STOP_FOREGROUND_REMOVE)
            stopSelf()
        }
    }

    private fun rejectStart(error: Throwable) {
        val mayFinish = synchronized(lifecycleLock) {
            canFinishRejectedStart(active != null, teardownInProgress)
        }
        if (!mayFinish) {
            Log.w(TAG, "Rejected malformed START without disturbing the current VPN session", error)
            return
        }
        finishWithoutResources(error)
    }

    private fun requestStop(
        expectedGeneration: Long? = null,
        sendStopped: (() -> Unit)? = null,
        failure: Throwable? = null,
    ) {
        var resourcesToClose: SessionResources? = null
        var runAcknowledgement: (() -> Unit)? = null
        var finishEmptyStop = false

        synchronized(lifecycleLock) {
            val matchesActive = stopTargetsGeneration(
                expectedGeneration,
                active?.generation,
                teardownInProgress,
                closingGeneration,
                expectedGeneration?.let(generationGate::isCurrent) == true,
            )
            if (!matchesActive) {
                return@synchronized
            }

            if (sendStopped != null) {
                when (stopAcknowledgements.register(sendStopped)) {
                    StopAcknowledgements.Registration.RUN_NOW -> runAcknowledgement = sendStopped
                    StopAcknowledgements.Registration.REJECTED -> Unit
                    StopAcknowledgements.Registration.QUEUED -> Unit
                }
            }

            if (failure != null) {
                terminalAfterTeardown = LifecycleState.ERROR
                lastError.set(failure.message ?: failure.javaClass.simpleName)
            }
            if (teardownInProgress) return@synchronized

            val resources = active
            if (resources == null) {
                expectedGeneration?.let(generationGate::invalidate)
                vpnDescriptorOpen.set(false)
                if (failure != null) {
                    state.set(LifecycleState.ERROR)
                } else if (state.get() != LifecycleState.ERROR) {
                    state.set(LifecycleState.STOPPED)
                    sessionId = null
                }
                finishEmptyStop = true
                return@synchronized
            }

            active = null
            generationGate.invalidate(resources.generation)
            teardownInProgress = true
            closingGeneration = resources.generation
            terminalAfterTeardown = if (failure == null) LifecycleState.STOPPED else LifecycleState.ERROR
            state.set(LifecycleState.STOPPING)
            resourcesToClose = resources
        }

        runAcknowledgement?.let(::invokeAcknowledgement)
        resourcesToClose?.let { worker.execute { teardown(it) } }
        if (finishEmptyStop) {
            mainHandler.post {
                stopForeground(STOP_FOREGROUND_REMOVE)
                stopSelf()
            }
        }
    }

    private fun teardown(resources: SessionResources) {
        val errors = mutableListOf<Throwable>()
        var descriptorClosed = false
        var engineStopped = false
        var stopRequested = false

        synchronized(resources.engineStartLock) {
            try {
                resources.tunnel.requestStop()
                stopRequested = true
            } catch (error: Throwable) {
                errors += error
            }
            if (stopRequested) {
                try {
                    engineStopped = resources.tunnel.awaitStopped(ENGINE_QUIESCE_BEFORE_CLOSE_TIMEOUT_MS)
                } catch (error: Throwable) {
                    errors += error
                }
            }
            try {
                resources.vpnInterface.close()
                descriptorClosed = true
                vpnDescriptorOpen.set(false)
            } catch (error: Throwable) {
                errors += error
                vpnDescriptorOpen.set(true)
            }
        }

        if (descriptorClosed) {
            stopAcknowledgements.descriptorClosed().forEach(::invokeAcknowledgement)
        } else {
            stopAcknowledgements.descriptorCloseFailed()
        }

        if (!engineStopped) {
            try {
                if (!resources.tunnel.awaitStopped(ENGINE_STOP_TIMEOUT_MS)) {
                    errors += IllegalStateException("HEV native engine did not stop within ${ENGINE_STOP_TIMEOUT_MS}ms after VPN closure")
                }
            } catch (error: Throwable) {
                errors += error
            }
        }
        try {
            resources.control?.close()
        } catch (error: Throwable) {
            errors += error
        }

        synchronized(lifecycleLock) {
            teardownInProgress = false
            closingGeneration = 0L
            val terminal = if (errors.isEmpty()) terminalAfterTeardown else LifecycleState.ERROR
            state.set(terminal)
            if (terminal == LifecycleState.STOPPED) sessionId = null
            if (errors.isNotEmpty()) {
                val detail = errors.joinToString("; ") { it.message ?: it.javaClass.simpleName }
                val existing = lastError.get()
                lastError.set(listOfNotNull(existing, detail).distinct().joinToString("; "))
                errors.forEach { Log.e(TAG, "Resource teardown failure", it) }
            }
        }

        if (destroyed.get()) worker.shutdown()

        mainHandler.post {
            updateNotification()
            stopForeground(STOP_FOREGROUND_REMOVE)
            stopSelf()
        }
    }

    private fun invokeAcknowledgement(acknowledgement: () -> Unit) {
        try {
            acknowledgement()
        } catch (error: Throwable) {
            Log.w(TAG, "Could not acknowledge STOP", error)
        }
    }

    override fun onRevoke() {
        requestStop()
        super.onRevoke()
    }

    override fun onDestroy() {
        destroyed.set(true)
        mainHandler.removeCallbacks(sleepStatePoller)
        if (displayListenerRegistered.compareAndSet(true, false)) {
            runCatching {
                getSystemService(DisplayManager::class.java).unregisterDisplayListener(displayListener)
            }.onFailure { Log.w(TAG, "Could not unregister display listener", it) }
        }
        if (screenReceiverRegistered.compareAndSet(true, false)) {
            runCatching { unregisterReceiver(screenReceiver) }
                .onFailure { Log.w(TAG, "Could not unregister screen receiver", it) }
        }
        requestStop(failure = if (state.get() == LifecycleState.ERROR) {
            IllegalStateException(lastError.get() ?: "VPN service terminated with an error")
        } else {
            null
        })
        synchronized(lifecycleLock) {
            if (!teardownInProgress) worker.shutdown()
        }
        super.onDestroy()
    }

    override fun onBind(intent: Intent?): IBinder? = super.onBind(intent)

    override fun dump(fd: FileDescriptor, writer: PrintWriter, args: Array<out String>) {
        val resources = synchronized(lifecycleLock) { active }
        val tunnel = resources?.tunnel
        val stats = runCatching { tunnel?.stats() }.getOrNull()
        val rtt = controlRtt.snapshot()
        writer.println("gnirehtet.state=${state.get()}")
        writer.println("vpnFdOpen=${vpnDescriptorOpen.get()}")
        writer.println("sessionId=${sessionId ?: "none"}")
        writer.println("lastError=${lastError.get() ?: "none"}")
        writer.println("screenSuspended=${screenSuspended.get()}")
        writer.println("socksPort=${resources?.parameters?.socksPort ?: "none"}")
        writer.println("udpPort=${resources?.parameters?.udpPort ?: "none"}")
        writer.println("controlPort=${resources?.parameters?.controlPort ?: "none"}")
        writer.println("controlRttSamples=${rtt.samples}")
        writer.println("controlRttP99Us=${rtt.p99Micros}")
        writer.println("controlRttMaxUs=${rtt.maxMicros}")
        writer.println("controlRttHistogram=${rtt.histogram.joinToString(",")}")
        if (stats != null && stats.size >= 4) {
            writer.println("txPackets=${stats[0]} txBytes=${stats[1]} rxPackets=${stats[2]} rxBytes=${stats[3]}")
        }
    }

    private fun buildNotification(): Notification {
        val manager = getSystemService(NotificationManager::class.java)
        manager.createNotificationChannel(
            NotificationChannel(CHANNEL_ID, getString(R.string.status_title), NotificationManager.IMPORTANCE_LOW),
        )
        val open = PendingIntent.getActivity(
            this,
            1,
            Intent(this, MainActivity::class.java),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val stop = PendingIntent.getService(
            this,
            2,
            Intent(this, VdLinkVpnService::class.java).setAction(ACTION_STOP),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        return Notification.Builder(this, CHANNEL_ID)
            .setSmallIcon(R.drawable.ic_usb_24)
            .setContentTitle(getString(R.string.status_title))
            .setContentText(state.get().name.lowercase())
            .setContentIntent(open)
            .setOngoing(true)
            .addAction(Notification.Action.Builder(null, getString(R.string.stop_link), stop).build())
            .build()
    }

    private fun startForegroundCompat(notification: Notification) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            startForeground(NOTIFICATION_ID, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE)
        } else {
            startForeground(NOTIFICATION_ID, notification)
        }
    }

    private fun updateNotification() {
        try {
            getSystemService(NotificationManager::class.java).notify(NOTIFICATION_ID, buildNotification())
        } catch (error: SecurityException) {
            Log.w(TAG, "Notification permission is unavailable", error)
        }
    }

    companion object {
        const val ACTION_START = "com.genymobile.gnirehtet.v4.START"
        const val ACTION_STOP = "com.genymobile.gnirehtet.v4.STOP"
        const val EXTRA_ALL_TRAFFIC = "allTraffic"
        const val EXTRA_VD_PACKAGE = "vdPackage"
        const val EXTRA_SOCKS_PORT = "socksPort"
        const val EXTRA_UDP_PORT = "udpPort"
        const val EXTRA_CONTROL_PORT = "controlPort"
        const val EXTRA_SESSION_ID = "sessionId"
        const val DEFAULT_VD_PACKAGE = "VirtualDesktop.Android"
        const val DEFAULT_ALL_TRAFFIC = true
        const val DEFAULT_SOCKS_PORT = 31_416
        const val DEFAULT_UDP_PORT = 31_418
        const val DEFAULT_CONTROL_PORT = 31_417
        const val MTU = 1_500

        val state: AtomicReference<LifecycleState> = AtomicReference(LifecycleState.STOPPED)
        val lastError: AtomicReference<String?> = AtomicReference(null)
        val vpnDescriptorOpen = AtomicBoolean(false)
        internal val controlRtt = ControlRttMetrics()

        private const val TAG = "VdLinkVpnService"
        private const val CHANNEL_ID = "wired-vd-link"
        private const val NOTIFICATION_ID = 31_416
        private const val ENGINE_START_TIMEOUT_MS = 5_000
        private const val ENGINE_QUIESCE_BEFORE_CLOSE_TIMEOUT_MS = 500
        private const val ENGINE_STOP_TIMEOUT_MS = 1_500
        private const val ENGINE_RESTART_RETRY_MS = 100L
        private const val SLEEP_STATE_POLL_INTERVAL_MS = 500L
        private const val WAKE_DISPLAY_DEBOUNCE_MS = 1_500L

        fun start(context: Context, source: Intent) {
            val intent = Intent(context, VdLinkVpnService::class.java)
                .setAction(ACTION_START)
                .putExtras(source)
            context.startForegroundService(intent)
        }

        fun stop(context: Context) {
            context.startService(
                Intent(context, VdLinkVpnService::class.java).setAction(ACTION_STOP),
            )
        }
    }

    private class LifecycleThreadFactory : ThreadFactory {
        private val sequence = AtomicInteger()

        override fun newThread(task: Runnable): Thread = Thread(
            task,
            "gnr4-lifecycle-${sequence.incrementAndGet()}",
        ).apply { isDaemon = true }
    }
}
