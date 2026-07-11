package org.communitybig.biglace.feature.terminal

import org.junit.Assert.assertTrue
import org.junit.Assert.assertFalse
import org.junit.Test

class TerminalEmulatorTest {
    @Test
    fun resizePreservesScreenAndScrollbackWithoutInvalidRows() {
        val terminal = TerminalEmulator(rows = 2, cols = 8)
        terminal.feed("first\r\nsecond\r\nthird")
        terminal.resize(newRows = 4, newCols = 20)
        val rendered = terminal.render().text
        assertTrue(rendered.contains("second"))
        assertTrue(rendered.contains("third"))

        terminal.resize(newRows = 2, newCols = 8)
        assertTrue(terminal.render().text.isNotEmpty())
    }

    @Test
    fun renderBoundsLongScrollbackToRecentRows() {
        val terminal = TerminalEmulator(rows = 2, cols = 16, scrollbackMax = 100)
        repeat(60) { terminal.feed("line-$it\r\n") }

        val rendered = terminal.render(scrollbackLimit = 10).text

        assertFalse(rendered.contains("line-0"))
        assertTrue(rendered.contains("line-59"))
        assertTrue(rendered.lineSequence().count() <= 12)
    }
}
