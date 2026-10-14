package org.communitybig.biglace.core.update

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class AppUpdaterTest {
    @Test
    fun parsesPublishedApkNames() {
        assertEquals(
            ApkAssetName("0.9.7", 34, "arm64-v8a"),
            parseApkAssetName("biglace-mobile-0.9.7-vc34-arm64-v8a.apk"),
        )
        assertEquals("x86_64", parseApkAssetName("biglace-mobile-1.0.0-rc1-vc40-x86_64.apk")?.abi)
        // CI builds are named after the date-based release tag.
        assertEquals(
            ApkAssetName("26.10.14-1230", 3569070, "arm64-v8a"),
            parseApkAssetName("biglace-mobile-26.10.14-1230-vc3569070-arm64-v8a.apk"),
        )
        assertNull(parseApkAssetName("biglace-mobile-0.9.7-vc34-arm64-v8a.apk.sha256"))
        assertNull(parseApkAssetName("biglace-26.09.01-0157-x86_64.pkg.tar.zst"))
    }

    @Test
    fun acceptsOnlyThisRepositorysReleaseDownloads() {
        assertTrue(isTrustedApkUrl("https://github.com/big-comm/biglace/releases/download/android-v0.9.7/a.apk"))
        assertFalse(isTrustedApkUrl("http://github.com/big-comm/biglace/releases/download/android-v0.9.7/a.apk"))
        assertFalse(isTrustedApkUrl("https://github.com/someone/biglace/releases/download/x/a.apk"))
        assertFalse(isTrustedApkUrl("https://github.com.evil.io/big-comm/biglace/releases/download/x/a.apk"))
    }

    @Test
    fun automaticChecksAreSpacedButNeverStuck() {
        val hour = 60 * 60 * 1000L
        assertTrue(isCheckDue(nowMs = 10 * hour, lastCheckMs = 0))
        assertFalse(isCheckDue(nowMs = 10 * hour, lastCheckMs = 10 * hour - 60_000))
        assertTrue(isCheckDue(nowMs = 10 * hour, lastCheckMs = 10 * hour - AUTO_CHECK_INTERVAL_MS))
        // A clock that moved backwards must not suppress checks until it catches up.
        assertTrue(isCheckDue(nowMs = 10 * hour, lastCheckMs = 20 * hour))
    }
}
