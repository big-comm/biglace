package org.communitybig.biglace.feature.files

import org.junit.Assert.assertEquals
import org.junit.Assert.assertThrows
import org.junit.Test

class FileHelpersTest {
    private val entries = listOf(
        RemoteFile("z-dir", true, 0),
        RemoteFile("small.txt", false, 10),
        RemoteFile("large.zip", false, 100),
    )

    @Test
    fun filtersCaseInsensitivelyAndKeepsDirectoriesFirst() {
        assertEquals(listOf("small.txt"), filterAndSortEntries(entries, "SMALL", FileSort.NAME, true).map { it.name })
        assertEquals(
            listOf("z-dir", "large.zip", "small.txt"),
            filterAndSortEntries(entries, "", FileSort.SIZE, false).map { it.name },
        )
    }

    @Test
    fun stagedFilenameCannotEscapeItsDirectory() {
        assertEquals("passwd", sanitizeStagedFilename("../../etc/passwd"))
        assertEquals("safe.txt", sanitizeStagedFilename("safe\u0000.txt"))
        assertThrows(IllegalArgumentException::class.java) { sanitizeStagedFilename("..") }
    }
}
