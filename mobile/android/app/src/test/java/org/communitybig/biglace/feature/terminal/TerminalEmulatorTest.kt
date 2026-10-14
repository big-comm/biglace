package org.communitybig.biglace.feature.terminal

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertSame
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

    @Test
    fun renderBoundsLongScrollbackToRecentRows() {
        val terminal = TerminalEmulator(rows = 2, cols = 16, scrollbackMax = 100)
        repeat(60) { terminal.feed("line-$it\r\n") }

        val rendered = terminal.render(scrollbackLimit = 10).text

        assertFalse(rendered.contains("line-0"))
        assertTrue(rendered.contains("line-59"))
        assertTrue(rendered.lineSequence().count() <= 12)
    }

    @Test
    fun shrinkingKeepsTheBottomOfTheScreenAndMovesTheTopToScrollback() {
        val terminal = TerminalEmulator(rows = 6, cols = 20)
        terminal.feed((1..6).joinToString("\r\n") { "line$it" })

        terminal.resize(newRows = 3, newCols = 20)

        assertEquals(listOf("line4", "line5", "line6"), terminal.snapshot(0).map { it.text.trim() })
        assertTrue(terminal.render().text.startsWith("line1"))
    }

    @Test
    fun privateAndStringSequencesAreConsumedNotPrinted() {
        val terminal = TerminalEmulator(rows = 4, cols = 40)
        terminal.feed("A\u001B[>4;2mB\u001B[>cC\u001B[>1uD")
        terminal.feed("\u001BP+q544e\u001B\\E\u001B]0;title\u0007F\u001B[2 qG")
        assertEquals("ABCDEFG", terminal.render().text.trim())
    }

    @Test
    fun decLineDrawingCharsetMapsToBoxCharacters() {
        val terminal = TerminalEmulator(rows = 4, cols = 40)
        terminal.feed("\u001B(0lqk~\u001B(Blqk")
        assertEquals("┌─┐·lqk", terminal.render().text.trim())
    }

    @Test
    fun tracksKeyboardModesAndAnswersCursorPositionReports() {
        val terminal = TerminalEmulator(rows = 4, cols = 40)
        val replies = ArrayList<String>()
        terminal.onReply = { replies.add(it) }

        terminal.feed("\u001B[?1h\u001B[?2004h")
        assertTrue(terminal.applicationCursorKeys)
        assertTrue(terminal.bracketedPaste)
        terminal.feed("\u001B[?1l")
        assertFalse(terminal.applicationCursorKeys)

        terminal.feed("\u001B[2;5H\u001B[6n")
        assertEquals(listOf("\u001B[2;5R"), replies)
    }

    @Test
    fun repeatAndColonColoursAreUnderstood() {
        val terminal = TerminalEmulator(rows = 4, cols = 40)
        terminal.feed("x\u001B[3b\u001B[38:2::255:0:0mR\u001B[0m")
        assertEquals("xxxxR", terminal.render().text.trim())
    }

    @Test
    fun unchangedLinesKeepTheirRenderedInstance() {
        val terminal = TerminalEmulator(rows = 4, cols = 20)
        terminal.feed("top\r\nmiddle\r\nbottom")
        val before = terminal.snapshot()

        terminal.feed("!")
        val after = terminal.snapshot()

        assertSame(before[0], after[0])
        assertSame(before[1], after[1])
        assertEquals("bottom!", after[2].text.trim())
    }
}
