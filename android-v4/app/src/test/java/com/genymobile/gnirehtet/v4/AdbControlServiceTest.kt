package com.genymobile.gnirehtet.v4

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class AdbControlServiceTest {
    @Test
    fun mapsOnlyTheExplicitControlActions() {
        assertEquals(AdbControlCommand.START, adbControlCommand(AdbControlActivity.ACTION_START))
        assertEquals(AdbControlCommand.STOP, adbControlCommand(AdbControlActivity.ACTION_STOP))
        assertEquals(AdbControlCommand.UNSUPPORTED, adbControlCommand(null))
        assertEquals(AdbControlCommand.UNSUPPORTED, adbControlCommand("unexpected"))
    }

    @Test
    fun adbStartRequiresOnlyPreparedVpnConsent() {
        assertTrue(canStartVpnFromAdb(vpnPermissionPrepared = true))
        assertFalse(canStartVpnFromAdb(vpnPermissionPrepared = false))
    }
}
