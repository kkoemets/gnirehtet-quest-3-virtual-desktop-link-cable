package com.genymobile.gnirehtet.v4

import android.app.Activity
import android.os.Bundle
import android.view.Gravity
import android.widget.Button
import android.widget.LinearLayout
import android.widget.TextView

class MainActivity : Activity() {
    private lateinit var status: TextView

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        status = TextView(this).apply {
            textSize = 20f
            gravity = Gravity.CENTER
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
                addView(stop)
            },
        )
    }

    override fun onResume() {
        super.onResume()
        refresh()
    }

    private fun refresh() {
        status.text = buildString {
            append(getString(R.string.status_title)).append('\n')
            append(VdLinkVpnService.state.get().name.lowercase())
            VdLinkVpnService.lastError.get()?.let { append("\n\n").append(it) }
        }
    }
}
