package com.genymobile.gnirehtet.v4

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Intent
import android.content.pm.ServiceInfo
import android.net.VpnService
import android.os.Build
import android.os.IBinder
import android.util.Log

internal enum class AdbControlCommand {
    START,
    STOP,
    UNSUPPORTED,
}

internal fun adbControlCommand(action: String?): AdbControlCommand = when (action) {
    AdbControlActivity.ACTION_START -> AdbControlCommand.START
    AdbControlActivity.ACTION_STOP -> AdbControlCommand.STOP
    else -> AdbControlCommand.UNSUPPORTED
}

internal fun canStartVpnFromAdb(vpnPermissionPrepared: Boolean): Boolean = vpnPermissionPrepared

class AdbControlService : Service() {
    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        startForegroundCompat(buildNotification())
        try {
            when (adbControlCommand(intent?.action)) {
                AdbControlCommand.START -> startVpn(intent!!)
                AdbControlCommand.STOP -> VdLinkVpnService.stop(this)
                AdbControlCommand.UNSUPPORTED -> recordError("Unsupported ADB control action")
            }
        } catch (error: RuntimeException) {
            Log.e(TAG, "ADB control dispatch failed", error)
            recordError(error.message ?: error.javaClass.simpleName)
        } finally {
            stopForeground(STOP_FOREGROUND_REMOVE)
            stopSelf(startId)
        }
        return START_NOT_STICKY
    }

    private fun startVpn(source: Intent) {
        if (!canStartVpnFromAdb(VpnService.prepare(this) == null)) {
            recordError(MANUAL_VPN_CONSENT_ERROR)
            return
        }
        VdLinkVpnService.start(this, source)
    }

    private fun recordError(message: String) {
        VdLinkVpnService.lastError.set(message)
        VdLinkVpnService.state.set(LifecycleState.ERROR)
    }

    private fun buildNotification(): Notification {
        getSystemService(NotificationManager::class.java).createNotificationChannel(
            NotificationChannel(
                CHANNEL_ID,
                getString(R.string.control_channel_name),
                NotificationManager.IMPORTANCE_LOW,
            ),
        )
        return Notification.Builder(this, CHANNEL_ID)
            .setSmallIcon(R.drawable.ic_usb_24)
            .setContentTitle(getString(R.string.control_notification_title))
            .setOngoing(true)
            .build()
    }

    private fun startForegroundCompat(notification: Notification) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            startForeground(
                NOTIFICATION_ID,
                notification,
                ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE,
            )
        } else {
            startForeground(NOTIFICATION_ID, notification)
        }
    }

    override fun onBind(intent: Intent?): IBinder? = null

    companion object {
        internal const val MANUAL_VPN_CONSENT_ERROR =
            "VPN consent is required; open Quest VD Wired on the headset and grant VPN permission"
        private const val TAG = "AdbControlService"
        private const val CHANNEL_ID = "wired-vd-control"
        private const val NOTIFICATION_ID = 31_417
    }
}
