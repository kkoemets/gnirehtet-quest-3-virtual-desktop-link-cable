package com.genymobile.gnirehtet.v4

import android.app.Activity
import android.content.Intent
import android.net.VpnService
import android.os.Bundle
import android.view.Gravity
import android.view.View
import android.widget.Button
import android.widget.LinearLayout
import android.widget.TextView

class MainActivity : Activity() {
    private lateinit var status: TextView
    private lateinit var prepareVpn: Button

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        status = TextView(this).apply {
            textSize = 20f
            gravity = Gravity.CENTER
        }
        prepareVpn = Button(this).apply {
            text = getString(R.string.grant_vpn_permission)
            setOnClickListener {
                startActivity(
                    Intent(this@MainActivity, AdbControlActivity::class.java)
                        .setAction(AdbControlActivity.ACTION_PREPARE),
                )
            }
        }
        val stop = Button(this).apply {
            text = getString(R.string.stop_link)
            setOnClickListener {
                VdLinkVpnService.stop(this@MainActivity)
                refresh()
            }
        }
        setContentView(
            LinearLayout(this).apply {
                orientation = LinearLayout.VERTICAL
                gravity = Gravity.CENTER
                setPadding(48, 48, 48, 48)
                addView(status)
                addView(prepareVpn)
                addView(stop)
            },
        )
    }

    override fun onResume() {
        super.onResume()
        refresh()
    }

    private fun refresh() {
        prepareVpn.visibility = if (VpnService.prepare(this) == null) View.GONE else View.VISIBLE
        status.text = buildString {
            append(getString(R.string.status_title)).append('\n')
            append(VdLinkVpnService.state.get().name.lowercase())
            VdLinkVpnService.lastError.get()?.let { append("\n\n").append(it) }
        }
    }
}
