package org.communitybig.biglace.feature.settings

import org.communitybig.biglace.core.mesh.MeshErrorKind
import org.communitybig.biglace.core.mesh.classifyMeshError
import org.communitybig.biglace.core.panel.isLoopbackHost
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
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

    @Test
    fun onlyThisDeviceCountsAsLoopback() {
        assertTrue(isLoopbackHost("localhost"))
        assertTrue(isLoopbackHost("127.0.0.1"))
        assertFalse(isLoopbackHost("127.0.0.1.example.com"))
        assertFalse(isLoopbackHost("100.64.0.1"))
    }

    @Test
    fun recognisesControlServerAuthKeyFailures() {
        assertEquals(
            MeshErrorKind.AUTHKEY_NOT_FOUND,
            classifyMeshError("tsnet.Up: backend: handling register with auth key: auth-key not found"),
        )
        assertEquals(MeshErrorKind.AUTHKEY_EXPIRED, classifyMeshError("register: authkey expired"))
        assertEquals(MeshErrorKind.TIMEOUT, classifyMeshError("tsnet.Up: context deadline exceeded"))
        assertEquals(null, classifyMeshError("something else entirely"))
    }
}
