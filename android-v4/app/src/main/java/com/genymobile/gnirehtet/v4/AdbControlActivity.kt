package com.genymobile.gnirehtet.v4

import android.Manifest
import android.app.Activity
import android.content.Intent
import android.content.pm.PackageManager
import android.net.VpnService
import android.os.Build
import android.os.Bundle

class AdbControlActivity : Activity() {
    private var pendingStart: Intent? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        when (intent?.action) {
            ACTION_STOP -> {
                VdLinkVpnService.stop(this)
                finish()
            }
            ACTION_START -> requestNotificationAndVpnPermission(intent)
            else -> finish()
        }
    }

    private fun requestNotificationAndVpnPermission(source: Intent) {
        pendingStart = Intent(source)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
            checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED
        ) {
            requestPermissions(arrayOf(Manifest.permission.POST_NOTIFICATIONS), NOTIFICATION_PERMISSION_REQUEST)
            return
        }
        requestVpnPermission()
    }

    private fun requestVpnPermission() {
        val permission = VpnService.prepare(this)
        if (permission == null) {
            pendingStart?.let { VdLinkVpnService.start(this, it) }
            pendingStart = null
            finish()
        } else {
            @Suppress("DEPRECATION")
            startActivityForResult(permission, VPN_PERMISSION_REQUEST)
        }
    }

    override fun onRequestPermissionsResult(
        requestCode: Int,
        permissions: Array<out String>,
        grantResults: IntArray,
    ) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults)
        if (requestCode != NOTIFICATION_PERMISSION_REQUEST) return
        if (grantResults.firstOrNull() == PackageManager.PERMISSION_GRANTED) {
            requestVpnPermission()
        } else {
            VdLinkVpnService.lastError.set("Notification permission is required for manual wired-link recovery")
            VdLinkVpnService.state.set(LifecycleState.ERROR)
            pendingStart = null
            finish()
        }
    }

    @Deprecated("VpnService permission still uses the activity result contract")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        if (requestCode == VPN_PERMISSION_REQUEST && resultCode == RESULT_OK) {
            pendingStart?.let { VdLinkVpnService.start(this, it) }
        }
        pendingStart = null
        finish()
    }

    companion object {
        const val ACTION_START = "com.genymobile.gnirehtet.v4.START"
        const val ACTION_STOP = "com.genymobile.gnirehtet.v4.STOP"
        private const val VPN_PERMISSION_REQUEST = 42
        private const val NOTIFICATION_PERMISSION_REQUEST = 43
    }
}
