package org.communitybig.biglace.feature.settings

import org.junit.Assert.assertEquals
import org.junit.Test

class DeviceNameTest {
    @Test
    fun normalizesToHealthyDnsLabel() {
        assertEquals("sala-pc-01", sanitizeDeviceName("  Sálá PC__01  "))
        assertEquals("phone", sanitizeDeviceName("---PHONE---"))
    }

    @Test
    fun limitsLabelTo63CharactersWithoutTrailingHyphen() {
        val value = sanitizeDeviceName("a".repeat(62) + " - ignored")
        assertEquals(62, value.length)
        assertEquals(false, value.endsWith('-'))
    }
}
