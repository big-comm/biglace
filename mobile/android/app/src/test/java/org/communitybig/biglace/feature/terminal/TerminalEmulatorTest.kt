package org.communitybig.biglace.feature.terminal

import org.junit.Assert.assertTrue
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
}
