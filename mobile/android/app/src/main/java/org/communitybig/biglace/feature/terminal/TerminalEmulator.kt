package org.communitybig.biglace.feature.terminal

import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.withStyle

/**
 * A grid-based ANSI/VT terminal emulator — the correct model for a real shell.
 * It keeps a fixed [rows]×[cols] screen the shell draws into (absolute cursor
 * positioning, erase, insert/delete, scroll regions, SGR colours, alt-screen for
 * full-screen apps like vim/htop) plus a scrollback buffer for lines that scroll
 * off the top. This is what makes coloured prompts (ble.sh/powerline) and TUIs
 * render correctly instead of scattering characters.
 */
class TerminalEmulator(
    rows: Int = 24,
    cols: Int = 80,
    private val scrollbackMax: Int = 800,
) {
    var rows: Int = rows.coerceAtLeast(2)
        private set
    var cols: Int = cols.coerceAtLeast(8)
        private set
    private class Cell(val ch: Char, val fg: Long, val bg: Long, val bold: Boolean)

    private var screen = blankGrid()
    private var mainScreen: Array<Array<Cell>>? = null
    private var inAlt = false
    private val scrollback = ArrayDeque<Array<Cell>>()

    private var cr = 0            // cursor row
    private var cc = 0            // cursor col
    private var savedR = 0
    private var savedC = 0
    private var top = 0           // scroll region top
    private var bottom = rows - 1 // scroll region bottom
    private var wrap = false      // deferred end-of-line wrap

    private var curFg = FG
    private var curBg = BG_NONE
    private var curBold = false

    private var state = NORMAL
    private val pb = StringBuilder()

    @Synchronized
    fun feed(text: String) {
        for (c in text) step(c)
    }

    private fun step(c: Char) {
        when (state) {
            NORMAL -> normal(c)
            ESC -> esc(c)
            CSI -> csi(c)
            OSC -> if (c == BEL) state = NORMAL else if (c == ESCC) state = OSC_ST
            OSC_ST -> state = NORMAL
            CHARSET -> state = NORMAL
        }
    }

    private fun normal(c: Char) {
        when (c) {
            ESCC -> state = ESC
            '\n', '\u000B', '\u000C' -> lineFeed()
            '\r' -> { cc = 0; wrap = false }
            '\b' -> { if (cc > 0) cc--; wrap = false }
            '\t' -> { cc = minOf(cols - 1, (cc / 8 + 1) * 8); wrap = false }
            BEL -> Unit
            else -> if (c.code >= 32) printChar(c)
        }
    }

    private fun esc(c: Char) {
        when (c) {
            '[' -> { state = CSI; pb.clear() }
            ']' -> { state = OSC; pb.clear() }
            '7' -> { saveCursor(); state = NORMAL }
            '8' -> { restoreCursor(); state = NORMAL }
            'M' -> { reverseIndex(); state = NORMAL }
            'c' -> { reset(); state = NORMAL }
            '(', ')', '*', '+' -> state = CHARSET
            else -> state = NORMAL
        }
    }

    private fun csi(c: Char) {
        if (c in '0'..'9' || c == ';' || c == '?' || c in ' '..'/') {
            pb.append(c)
            return
        }
        handleCsi(c, pb.toString())
        state = NORMAL
    }

    private fun printChar(c: Char) {
        if (wrap) { cc = 0; lineFeed(); wrap = false }
        screen[cr][cc] = Cell(c, curFg, curBg, curBold)
        if (cc == cols - 1) wrap = true else cc++
    }

    private fun lineFeed() {
        if (cr == bottom) scrollUp() else if (cr < rows - 1) cr++
        wrap = false
    }

    private fun reverseIndex() {
        if (cr == top) scrollDown() else if (cr > 0) cr--
    }

    private fun scrollUp() {
        if (top == 0 && !inAlt) {
            scrollback.addLast(screen[top])
            while (scrollback.size > scrollbackMax) scrollback.removeFirst()
        }
        for (r in top until bottom) screen[r] = screen[r + 1]
        screen[bottom] = blankRow()
    }

    private fun scrollDown() {
        for (r in bottom downTo top + 1) screen[r] = screen[r - 1]
        screen[top] = blankRow()
    }

    private fun handleCsi(final: Char, raw: String) {
        val priv = raw.startsWith('?')
        val nums = raw.trimStart('?').split(';').map { it.toIntOrNull() }
        fun p(i: Int, def: Int) = nums.getOrNull(i) ?: def
        val n = maxOf(1, p(0, 1))
        when (final) {
            'A' -> { cr = maxOf(top, cr - n); wrap = false }
            'B' -> { cr = minOf(bottom, cr + n); wrap = false }
            'C' -> { cc = minOf(cols - 1, cc + n); wrap = false }
            'D' -> { cc = maxOf(0, cc - n); wrap = false }
            'E' -> { cr = minOf(bottom, cr + n); cc = 0 }
            'F' -> { cr = maxOf(top, cr - n); cc = 0 }
            'G', '`' -> { cc = (p(0, 1) - 1).coerceIn(0, cols - 1); wrap = false }
            'd' -> { cr = (p(0, 1) - 1).coerceIn(0, rows - 1); wrap = false }
            'H', 'f' -> {
                cr = (p(0, 1) - 1).coerceIn(0, rows - 1)
                cc = (p(1, 1) - 1).coerceIn(0, cols - 1)
                wrap = false
            }
            'J' -> eraseDisplay(p(0, 0))
            'K' -> eraseLine(cr, p(0, 0))
            'X' -> { var i = cc; repeat(n) { if (i < cols) screen[cr][i++] = blank() } }
            'P' -> deleteChars(n)
            '@' -> insertChars(n)
            'L' -> insertLines(n)
            'M' -> deleteLines(n)
            'r' -> {
                top = (p(0, 1) - 1).coerceIn(0, rows - 1)
                bottom = (p(1, rows) - 1).coerceIn(0, rows - 1)
                if (top >= bottom) { top = 0; bottom = rows - 1 }
                cr = top; cc = 0
            }
            's' -> saveCursor()
            'u' -> restoreCursor()
            'm' -> sgr(nums)
            'h' -> if (priv) setModes(nums, true)
            'l' -> if (priv) setModes(nums, false)
        }
    }

    private fun eraseDisplay(mode: Int) {
        when (mode) {
            0 -> { eraseLine(cr, 0); for (r in cr + 1 until rows) clearRow(r) }
            1 -> { for (r in 0 until cr) clearRow(r); eraseLine(cr, 1) }
            2, 3 -> { for (r in 0 until rows) clearRow(r); if (mode == 3) scrollback.clear() }
        }
    }

    private fun eraseLine(row: Int, mode: Int) {
        val line = screen[row]
        when (mode) {
            0 -> for (i in cc until cols) line[i] = blank()
            1 -> for (i in 0..minOf(cc, cols - 1)) line[i] = blank()
            2 -> for (i in 0 until cols) line[i] = blank()
        }
    }

    private fun deleteChars(n: Int) {
        val line = screen[cr]
        val count = minOf(n, cols - cc)
        for (i in cc until cols) line[i] = if (i + count < cols) line[i + count] else blank()
    }

    private fun insertChars(n: Int) {
        val line = screen[cr]
        val count = minOf(n, cols - cc)
        for (i in cols - 1 downTo cc) line[i] = if (i - count >= cc) line[i - count] else blank()
    }

    private fun insertLines(n: Int) {
        if (cr < top || cr > bottom) return
        repeat(minOf(n, bottom - cr + 1)) {
            for (r in bottom downTo cr + 1) screen[r] = screen[r - 1]
            screen[cr] = blankRow()
        }
    }

    private fun deleteLines(n: Int) {
        if (cr < top || cr > bottom) return
        repeat(minOf(n, bottom - cr + 1)) {
            for (r in cr until bottom) screen[r] = screen[r + 1]
            screen[bottom] = blankRow()
        }
    }

    private fun sgr(nums: List<Int?>) {
        if (nums.isEmpty() || (nums.size == 1 && nums[0] == null)) { resetSgr(); return }
        var i = 0
        val codes = nums.map { it ?: 0 }
        while (i < codes.size) {
            when (val n = codes[i]) {
                0 -> resetSgr()
                1 -> curBold = true
                22 -> curBold = false
                7 -> { val t = curFg; curFg = if (curBg == BG_NONE) 0xFF0B0E14L else curBg; curBg = t } // reverse video
                in 30..37 -> curFg = ANSI16[n - 30]
                in 90..97 -> curFg = ANSI16[n - 90 + 8]
                39 -> curFg = FG
                in 40..47 -> curBg = ANSI16[n - 40]
                in 100..107 -> curBg = ANSI16[n - 100 + 8]
                49 -> curBg = BG_NONE
                38 -> when (codes.getOrNull(i + 1)) {
                    5 -> { curFg = xterm256(codes.getOrNull(i + 2) ?: 7); i += 2 }
                    2 -> { curFg = rgb(codes, i); i += 4 }
                }
                48 -> when (codes.getOrNull(i + 1)) {
                    5 -> { curBg = xterm256(codes.getOrNull(i + 2) ?: 0); i += 2 }
                    2 -> { curBg = rgb(codes, i); i += 4 }
                }
            }
            i++
        }
    }

    private fun resetSgr() { curFg = FG; curBg = BG_NONE; curBold = false }

    private fun rgb(codes: List<Int>, i: Int): Long {
        val r = codes.getOrNull(i + 2) ?: 0
        val g = codes.getOrNull(i + 3) ?: 0
        val b = codes.getOrNull(i + 4) ?: 0
        return 0xFF000000L or (r.toLong() shl 16) or (g.toLong() shl 8) or b.toLong()
    }

    private fun setModes(nums: List<Int?>, on: Boolean) {
        for (m in nums) when (m) {
            47, 1047, 1049 -> if (on) enterAlt() else leaveAlt()
        }
    }

    private fun enterAlt() {
        if (inAlt) return
        mainScreen = screen
        savedR = cr; savedC = cc
        screen = blankGrid()
        cr = 0; cc = 0; top = 0; bottom = rows - 1; inAlt = true
    }

    private fun leaveAlt() {
        if (!inAlt) return
        screen = mainScreen ?: blankGrid()
        mainScreen = null
        cr = savedR; cc = savedC; top = 0; bottom = rows - 1; inAlt = false
    }

    private fun saveCursor() { savedR = cr; savedC = cc }
    private fun restoreCursor() { cr = savedR.coerceIn(0, rows - 1); cc = savedC.coerceIn(0, cols - 1) }

    private fun reset() {
        screen = blankGrid(); scrollback.clear(); mainScreen = null; inAlt = false
        cr = 0; cc = 0; top = 0; bottom = rows - 1; resetSgr(); wrap = false
    }

    @Synchronized
    fun clear() = reset()

    @Synchronized
    fun resize(newRows: Int, newCols: Int) {
        val targetRows = newRows.coerceIn(2, 200)
        val targetCols = newCols.coerceIn(8, 300)
        if (targetRows == rows && targetCols == cols) return

        fun resizedRow(source: Array<Cell>): Array<Cell> =
            Array(targetCols) { c -> source.getOrNull(c) ?: blank() }

        fun resized(source: Array<Array<Cell>>): Array<Array<Cell>> {
            val result = Array(targetRows) { Array(targetCols) { blank() } }
            for (r in 0 until minOf(source.size, targetRows)) {
                for (c in 0 until minOf(source[r].size, targetCols)) {
                    result[r][c] = source[r][c]
                }
            }
            return result
        }

        screen = resized(screen)
        mainScreen = mainScreen?.let(::resized)
        val resizedScrollback = ArrayDeque<Array<Cell>>(scrollback.size)
        scrollback.forEach { resizedScrollback.addLast(resizedRow(it)) }
        scrollback.clear()
        scrollback.addAll(resizedScrollback)
        rows = targetRows
        cols = targetCols
        cr = cr.coerceIn(0, rows - 1)
        cc = cc.coerceIn(0, cols - 1)
        savedR = savedR.coerceIn(0, rows - 1)
        savedC = savedC.coerceIn(0, cols - 1)
        top = 0
        bottom = rows - 1
        wrap = false
    }

    // ── Rendering ─────────────────────────────────────────────────────────────

    @Synchronized
    fun render(scrollbackLimit: Int = DEFAULT_RENDER_SCROLLBACK): AnnotatedString = buildAnnotatedString {
        val out = ArrayList<Pair<Array<Cell>, Int>>() // line + cursor col (-1 = none)
        if (!inAlt) {
            val skip = (scrollback.size - scrollbackLimit.coerceAtLeast(0)).coerceAtLeast(0)
            scrollback.asSequence().drop(skip).forEach { out.add(it to -1) }
        }
        // Only render screen rows down to the last non-blank row (or the cursor):
        // otherwise the 24-row grid's trailing blank lines push the prompt off the
        // top when we auto-scroll to the bottom.
        var lastRow = cr
        for (r in rows - 1 downTo 0) { if (rowNonBlank(screen[r])) { lastRow = maxOf(cr, r); break } }
        for (r in 0..lastRow) out.add(screen[r] to if (r == cr) cc else -1)

        for ((idx, pair) in out.withIndex()) {
            val line = pair.first
            val cursorCol = pair.second
            var last = -1
            for (i in cols - 1 downTo 0) { if (!isBlank(line[i])) { last = i; break } }
            val end = maxOf(last, cursorCol)
            var i = 0
            while (i <= end) {
                if (i == cursorCol) {
                    withStyle(SpanStyle(color = Color(0xFF0B0E14), background = CURSOR)) {
                        append(if (line[i].ch == ' ') ' ' else line[i].ch)
                    }
                    i++
                    continue
                }
                val fg = line[i].fg
                val bg = line[i].bg
                val bold = line[i].bold
                val sb = StringBuilder()
                while (i <= end && i != cursorCol && line[i].fg == fg && line[i].bg == bg && line[i].bold == bold) {
                    sb.append(line[i].ch); i++
                }
                val weight = if (bold) FontWeight.Bold else FontWeight.Normal
                val style = if (bg == BG_NONE) SpanStyle(color = Color(fg), fontWeight = weight)
                    else SpanStyle(color = Color(fg), background = Color(bg), fontWeight = weight)
                withStyle(style) { append(sb.toString()) }
            }
            if (idx < out.size - 1) append('\n')
        }
    }

    private fun isBlank(c: Cell) = c.ch == ' ' && c.fg == FG && c.bg == BG_NONE
    private fun rowNonBlank(row: Array<Cell>) = row.any { !isBlank(it) }

    // ── Grid helpers ──────────────────────────────────────────────────────────

    private fun blank() = Cell(' ', FG, BG_NONE, false)
    private fun blankRow() = Array(cols) { blank() }
    private fun blankGrid() = Array(rows) { blankRow() }
    private fun clearRow(r: Int) { screen[r] = blankRow() }

    companion object {
        private const val NORMAL = 0
        private const val ESC = 1
        private const val CSI = 2
        private const val OSC = 3
        private const val OSC_ST = 4
        private const val CHARSET = 5
        private const val DEFAULT_RENDER_SCROLLBACK = 200

        private const val ESCC = '\u001B'
        private const val BEL = '\u0007'

        private const val FG = 0xFFD7DCE5L
        private const val BG_NONE = 0L // sentinel: "no explicit background"
        private val CURSOR = Color(0xFF6BE58A)

        private val ANSI16 = longArrayOf(
            0xFF1B1D23, 0xFFE05561, 0xFF8CC265, 0xFFD18F52,
            0xFF4AA5F0, 0xFFC162DE, 0xFF42B3C2, 0xFFD7DCE5,
            0xFF5C6370, 0xFFFF616E, 0xFFA5E075, 0xFFF0A45D,
            0xFF4DC4FF, 0xFFDE73FF, 0xFF4CD1E0, 0xFFFFFFFF,
        )

        private fun xterm256(n: Int): Long = when {
            n < 16 -> ANSI16[n.coerceIn(0, 15)]
            n in 16..231 -> {
                val v = n - 16
                val r = v / 36; val g = (v % 36) / 6; val b = v % 6
                fun s(x: Int) = if (x == 0) 0L else 55L + x * 40
                0xFF000000L or (s(r) shl 16) or (s(g) shl 8) or s(b)
            }
            else -> { val g = (8 + (n - 232) * 10).toLong(); 0xFF000000L or (g shl 16) or (g shl 8) or g }
        }
    }
}
