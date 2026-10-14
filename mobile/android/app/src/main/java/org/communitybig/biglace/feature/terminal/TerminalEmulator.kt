package org.communitybig.biglace.feature.terminal

import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.font.FontStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextDecoration

/**
 * A grid-based ANSI/VT terminal emulator — the correct model for a real shell.
 * It keeps a fixed [rows]×[cols] screen the shell draws into (absolute cursor
 * positioning, erase, insert/delete, scroll regions, SGR colours, alt-screen for
 * full-screen apps like vim/htop) plus a scrollback buffer for lines that scroll
 * off the top. This is what makes coloured prompts (ble.sh/powerline) and TUIs
 * render correctly instead of scattering characters.
 *
 * Rendering is per line and cached: [snapshot] only rebuilds the lines that
 * changed since the previous call, so a TUI repainting a few rows many times a
 * second (Claude Code, htop) costs a few rows of text work, not the whole buffer.
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

    /** Terminal → host replies (cursor position reports). Invoked while feeding. */
    @Volatile var onReply: ((String) -> Unit)? = null

    /** DECCKM: arrow keys must be sent as `ESC O x` instead of `ESC [ x`. */
    @Volatile var applicationCursorKeys = false
        private set

    /** Mode 2004: pasted text must be wrapped in `ESC[200~` … `ESC[201~`. */
    @Volatile var bracketedPaste = false
        private set

    /** Mode 2026: the app is mid-repaint; presenting now would show a torn frame. */
    @Volatile var synchronizedOutput = false
        private set

    private class Cell(val ch: Char, val fg: Long, val bg: Long, val flags: Int)

    /** One screen row plus its rendered form, dropped whenever a cell changes. */
    private class Line(val cells: Array<Cell>) {
        var text: AnnotatedString? = null
    }

    private var screen = blankGrid()
    private var mainScreen: Array<Line>? = null
    private var inAlt = false
    private val scrollback = ArrayDeque<Line>()

    private var cr = 0            // cursor row
    private var cc = 0            // cursor col
    private var savedR = 0
    private var savedC = 0
    private var mainR = 0         // main-screen cursor while the alt screen is up
    private var mainC = 0
    private var top = 0           // scroll region top
    private var bottom = this.rows - 1 // scroll region bottom
    private var wrap = false      // deferred end-of-line wrap
    private var autoWrap = true
    private var cursorVisible = true

    private var curFg = FG
    private var curBg = BG_NONE
    private var curFlags = 0

    private var g0Graphics = false
    private var g1Graphics = false
    private var shiftOut = false
    private var charsetSlot = 0
    private var lastChar = ' '

    private var state = NORMAL
    private val params = StringBuilder()
    private var intermediate = false

    @Synchronized
    fun feed(text: String) {
        for (c in text) step(c)
    }

    private fun step(c: Char) {
        when (state) {
            NORMAL -> normal(c)
            ESC -> esc(c)
            CSI -> csi(c)
            STRING -> if (c == BEL) state = NORMAL else if (c == ESCC) state = STRING_ESC
            // ST is ESC \. Anything else means the terminator was lost: treat the
            // ESC as the start of a new sequence instead of swallowing the stream.
            STRING_ESC -> if (c == '\\') state = NORMAL else { state = ESC; esc(c) }
            CHARSET -> {
                if (charsetSlot == 0) g0Graphics = c == '0' else if (charsetSlot == 1) g1Graphics = c == '0'
                state = NORMAL
            }
            SKIP -> state = NORMAL
        }
    }

    private fun normal(c: Char) {
        when (c) {
            ESCC -> state = ESC
            '\n', '\u000B', '\u000C' -> lineFeed()
            '\r' -> { cc = 0; wrap = false }
            '\b' -> { if (cc > 0) cc--; wrap = false }
            '\t' -> { cc = minOf(cols - 1, (cc / 8 + 1) * 8); wrap = false }
            '\u000E' -> shiftOut = true
            '\u000F' -> shiftOut = false
            else -> if (c.code >= 32 && c != '\u007F') printChar(c)
        }
    }

    private fun esc(c: Char) {
        state = NORMAL
        when (c) {
            '[' -> { state = CSI; params.clear(); intermediate = false }
            // OSC, DCS, SOS, PM, APC: string payloads we don't interpret.
            ']', 'P', 'X', '^', '_' -> state = STRING
            '7' -> saveCursor()
            '8' -> restoreCursor()
            'D' -> lineFeed()
            'E' -> { cc = 0; lineFeed() }
            'M' -> reverseIndex()
            'c' -> reset()
            '(', ')', '*', '+' -> { charsetSlot = c - '('; state = CHARSET }
            ' ', '#', '%' -> state = SKIP
        }
    }

    private fun csi(c: Char) {
        when (c) {
            in '0'..'?' -> if (params.length < MAX_PARAM_CHARS) params.append(c)
            in ' '..'/' -> intermediate = true
            in '@'..'~' -> {
                state = NORMAL
                if (!intermediate) handleCsi(c, params.toString())
            }
            ESCC -> state = ESC
        }
    }

    private fun printChar(c: Char) {
        val graphics = if (shiftOut) g1Graphics else g0Graphics
        put(if (graphics && c in '`'..'~') ACS[c - '`'] else c)
    }

    private fun put(ch: Char) {
        if (wrap) { cc = 0; lineFeed() }
        val cell = if (ch == ' ' && curFg == FG && curBg == BG_NONE && curFlags == 0) BLANK
            else Cell(ch, curFg, curBg, curFlags)
        set(screen[cr], cc, cell)
        lastChar = ch
        if (cc == cols - 1) wrap = autoWrap else cc++
    }

    private fun set(line: Line, col: Int, cell: Cell) {
        line.cells[col] = cell
        line.text = null
    }

    private fun lineFeed() {
        if (cr == bottom) scrollUp() else if (cr < rows - 1) cr++
        wrap = false
    }

    private fun reverseIndex() {
        if (cr == top) scrollDown() else if (cr > 0) cr--
        wrap = false
    }

    private fun scrollUp() {
        if (top == 0 && !inAlt) pushScrollback(screen[top])
        for (r in top until bottom) screen[r] = screen[r + 1]
        screen[bottom] = newLine()
    }

    private fun scrollDown() {
        for (r in bottom downTo top + 1) screen[r] = screen[r - 1]
        screen[top] = newLine()
    }

    private fun pushScrollback(line: Line) {
        scrollback.addLast(line)
        while (scrollback.size > scrollbackMax) scrollback.removeFirst()
    }

    private fun handleCsi(final: Char, raw: String) {
        val marker = raw.firstOrNull()?.takeIf { it in '<'..'?' }
        val body = if (marker != null) raw.substring(1) else raw
        // '<', '=', '>' introduce private queries/extensions (device attributes,
        // modifyOtherKeys, kitty keyboard) that this terminal doesn't implement.
        if (marker != null && marker != '?') return
        if (marker == null && final == 'm') { sgr(body); return }

        val nums = body.split(';').map { it.substringBefore(':').toIntOrNull()?.coerceAtMost(MAX_PARAM) }
        fun p(i: Int, def: Int) = nums.getOrNull(i) ?: def
        if (marker == '?') {
            when (final) {
                'h' -> setModes(nums, true)
                'l' -> setModes(nums, false)
            }
            return
        }
        val n = maxOf(1, p(0, 1))
        when (final) {
            'A' -> { cr = maxOf(if (cr >= top) top else 0, cr - n); wrap = false }
            'B' -> { cr = minOf(if (cr <= bottom) bottom else rows - 1, cr + n); wrap = false }
            'C', 'a' -> { cc = minOf(cols - 1, cc + n); wrap = false }
            'D' -> { cc = maxOf(0, cc - n); wrap = false }
            'E' -> { cr = minOf(if (cr <= bottom) bottom else rows - 1, cr + n); cc = 0; wrap = false }
            'F' -> { cr = maxOf(if (cr >= top) top else 0, cr - n); cc = 0; wrap = false }
            'G', '`' -> { cc = (p(0, 1) - 1).coerceIn(0, cols - 1); wrap = false }
            'd' -> { cr = (p(0, 1) - 1).coerceIn(0, rows - 1); wrap = false }
            'H', 'f' -> {
                cr = (p(0, 1) - 1).coerceIn(0, rows - 1)
                cc = (p(1, 1) - 1).coerceIn(0, cols - 1)
                wrap = false
            }
            'J' -> eraseDisplay(p(0, 0))
            'K' -> eraseLine(cr, p(0, 0))
            'X' -> eraseCells(screen[cr], cc, minOf(cols, cc + n))
            'P' -> deleteChars(n)
            '@' -> insertChars(n)
            'L' -> insertLines(n)
            'M' -> deleteLines(n)
            'S' -> repeat(minOf(n, rows)) { scrollUp() }
            'T' -> repeat(minOf(n, rows)) { scrollDown() }
            'b' -> repeat(minOf(n, rows * cols)) { put(lastChar) }
            'r' -> {
                top = (p(0, 1) - 1).coerceIn(0, rows - 1)
                bottom = (p(1, rows) - 1).coerceIn(0, rows - 1)
                if (top >= bottom) { top = 0; bottom = rows - 1 }
                cr = top; cc = 0; wrap = false
            }
            's' -> saveCursor()
            'u' -> restoreCursor()
            'n' -> when (p(0, 0)) {
                5 -> onReply?.invoke("\u001B[0n")
                6 -> onReply?.invoke("\u001B[${cr + 1};${cc + 1}R")
            }
        }
    }

    /** Erased cells take the current background (terminfo `bce`), like xterm. */
    private fun eraseCell(): Cell = if (curBg == BG_NONE) BLANK else Cell(' ', FG, curBg, 0)

    private fun eraseCells(line: Line, from: Int, until: Int) {
        val cell = eraseCell()
        for (i in from until until) line.cells[i] = cell
        line.text = null
    }

    private fun eraseDisplay(mode: Int) {
        when (mode) {
            0 -> { eraseLine(cr, 0); for (r in cr + 1 until rows) eraseLine(r, 2) }
            1 -> { for (r in 0 until cr) eraseLine(r, 2); eraseLine(cr, 1) }
            2, 3 -> { for (r in 0 until rows) eraseLine(r, 2); if (mode == 3) scrollback.clear() }
        }
    }

    private fun eraseLine(row: Int, mode: Int) {
        val line = screen[row]
        when (mode) {
            0 -> eraseCells(line, cc, cols)
            1 -> eraseCells(line, 0, minOf(cc, cols - 1) + 1)
            2 -> eraseCells(line, 0, cols)
        }
    }

    private fun deleteChars(n: Int) {
        val line = screen[cr]
        val count = minOf(n, cols - cc)
        val fill = eraseCell()
        for (i in cc until cols) line.cells[i] = if (i + count < cols) line.cells[i + count] else fill
        line.text = null
    }

    private fun insertChars(n: Int) {
        val line = screen[cr]
        val count = minOf(n, cols - cc)
        val fill = eraseCell()
        for (i in cols - 1 downTo cc) line.cells[i] = if (i - count >= cc) line.cells[i - count] else fill
        line.text = null
    }

    private fun insertLines(n: Int) {
        if (cr < top || cr > bottom) return
        repeat(minOf(n, bottom - cr + 1)) {
            for (r in bottom downTo cr + 1) screen[r] = screen[r - 1]
            screen[cr] = newLine()
        }
    }

    private fun deleteLines(n: Int) {
        if (cr < top || cr > bottom) return
        repeat(minOf(n, bottom - cr + 1)) {
            for (r in cr until bottom) screen[r] = screen[r + 1]
            screen[bottom] = newLine()
        }
    }

    private fun sgr(body: String) {
        if (body.isEmpty()) { resetSgr(); return }
        val groups = body.split(';')
        fun int(i: Int) = groups.getOrNull(i)?.toIntOrNull()
        var i = 0
        while (i < groups.size) {
            val group = groups[i]
            if (':' in group) {
                // ITU-style sub-parameters: 38:2::r:g:b, 38:5:n, 4:3 …
                val sub = group.split(':').map { it.toIntOrNull() }
                when (sub[0]) {
                    38, 48 -> {
                        val color = when (sub.getOrNull(1)) {
                            5 -> xterm256(sub.getOrNull(2) ?: 0)
                            2 -> if (sub.size >= 5) sub.takeLast(3).let { rgb(it[0], it[1], it[2]) } else null
                            else -> null
                        }
                        if (color != null) { if (sub[0] == 38) curFg = color else curBg = color }
                    }
                    4 -> curFlags = if (sub.getOrNull(1) == 0) curFlags and UNDERLINE.inv() else curFlags or UNDERLINE
                }
                i++
                continue
            }
            when (val n = group.toIntOrNull() ?: 0) {
                0 -> resetSgr()
                1 -> curFlags = curFlags or BOLD
                2 -> curFlags = curFlags or DIM
                3 -> curFlags = curFlags or ITALIC
                4 -> curFlags = curFlags or UNDERLINE
                7 -> curFlags = curFlags or INVERSE
                22 -> curFlags = curFlags and (BOLD or DIM).inv()
                23 -> curFlags = curFlags and ITALIC.inv()
                24 -> curFlags = curFlags and UNDERLINE.inv()
                27 -> curFlags = curFlags and INVERSE.inv()
                in 30..37 -> curFg = ANSI16[n - 30]
                in 90..97 -> curFg = ANSI16[n - 90 + 8]
                39 -> curFg = FG
                in 40..47 -> curBg = ANSI16[n - 40]
                in 100..107 -> curBg = ANSI16[n - 100 + 8]
                49 -> curBg = BG_NONE
                38, 48 -> {
                    val color = when (int(i + 1)) {
                        5 -> xterm256(int(i + 2) ?: 0).also { i += 2 }
                        2 -> rgb(int(i + 2), int(i + 3), int(i + 4)).also { i += 4 }
                        else -> null
                    }
                    if (color != null) { if (n == 38) curFg = color else curBg = color }
                }
            }
            i++
        }
    }

    private fun resetSgr() { curFg = FG; curBg = BG_NONE; curFlags = 0 }

    private fun rgb(r: Int?, g: Int?, b: Int?): Long {
        fun ch(v: Int?) = (v ?: 0).coerceIn(0, 255).toLong()
        return 0xFF000000L or (ch(r) shl 16) or (ch(g) shl 8) or ch(b)
    }

    private fun setModes(nums: List<Int?>, on: Boolean) {
        for (m in nums) when (m) {
            1 -> applicationCursorKeys = on
            7 -> { autoWrap = on; if (!on) wrap = false }
            25 -> cursorVisible = on
            47, 1047, 1049 -> if (on) enterAlt() else leaveAlt()
            2004 -> bracketedPaste = on
            2026 -> synchronizedOutput = on
        }
    }

    private fun enterAlt() {
        if (inAlt) return
        mainScreen = screen
        mainR = cr; mainC = cc
        screen = blankGrid()
        cr = 0; cc = 0; top = 0; bottom = rows - 1; wrap = false; inAlt = true
    }

    private fun leaveAlt() {
        if (!inAlt) return
        screen = mainScreen ?: blankGrid()
        mainScreen = null
        cr = mainR.coerceIn(0, rows - 1); cc = mainC.coerceIn(0, cols - 1)
        top = 0; bottom = rows - 1; wrap = false; inAlt = false
    }

    private fun saveCursor() { savedR = cr; savedC = cc }
    private fun restoreCursor() {
        cr = savedR.coerceIn(0, rows - 1); cc = savedC.coerceIn(0, cols - 1); wrap = false
    }

    private fun reset() {
        screen = blankGrid(); scrollback.clear(); mainScreen = null; inAlt = false
        cr = 0; cc = 0; top = 0; bottom = rows - 1; resetSgr(); wrap = false
        autoWrap = true; cursorVisible = true
        g0Graphics = false; g1Graphics = false; shiftOut = false
        applicationCursorKeys = false; bracketedPaste = false; synchronizedOutput = false
    }

    @Synchronized
    fun clear() {
        reset()
        state = NORMAL
    }

    @Synchronized
    fun resize(newRows: Int, newCols: Int) {
        val targetRows = newRows.coerceIn(2, 200)
        val targetCols = newCols.coerceIn(8, 300)
        if (targetRows == rows && targetCols == cols) return

        fun resizedLine(source: Line?): Line = when {
            source == null -> newLine(targetCols)
            source.cells.size == targetCols -> source
            else -> Line(Array(targetCols) { source.cells.getOrNull(it) ?: BLANK })
        }

        // A shell screen is anchored at the bottom: when it gets shorter (the
        // soft keyboard opens) the rows that no longer fit leave through the top
        // into the scrollback, so the prompt and the latest output stay put.
        fun fitMain(source: Array<Line>, cursorRow: Int): Pair<Array<Line>, Int> {
            var used = cursorRow
            for (r in source.lastIndex downTo cursorRow + 1) {
                if (rowNonBlank(source[r])) { used = r; break }
            }
            val drop = (used + 1 - targetRows).coerceAtLeast(0)
            for (r in 0 until drop) pushScrollback(source[r])
            return Array(targetRows) { resizedLine(source.getOrNull(it + drop)) } to
                (cursorRow - drop).coerceAtLeast(0)
        }

        val main = mainScreen
        if (inAlt && main != null) {
            // Full-screen apps repaint on SIGWINCH; just crop/extend their canvas.
            screen = Array(targetRows) { resizedLine(screen.getOrNull(it)) }
            val (grid, row) = fitMain(main, mainR)
            mainScreen = grid
            mainR = row
        } else {
            val (grid, row) = fitMain(screen, cr)
            screen = grid
            cr = row
        }
        rows = targetRows
        cols = targetCols
        cr = cr.coerceIn(0, rows - 1)
        cc = cc.coerceIn(0, cols - 1)
        savedR = savedR.coerceIn(0, rows - 1)
        savedC = savedC.coerceIn(0, cols - 1)
        mainR = mainR.coerceIn(0, rows - 1)
        mainC = mainC.coerceIn(0, cols - 1)
        top = 0
        bottom = rows - 1
        wrap = false
    }

    // ── Rendering ─────────────────────────────────────────────────────────────

    /**
     * The visible buffer, one entry per line: up to [scrollbackLimit] scrollback
     * lines followed by the screen. Unchanged lines return the same instance as
     * the previous call, which lets the UI skip them entirely.
     */
    @Synchronized
    fun snapshot(scrollbackLimit: Int = Int.MAX_VALUE): List<AnnotatedString> {
        val out = ArrayList<AnnotatedString>()
        if (!inAlt) {
            val skip = (scrollback.size - scrollbackLimit.coerceAtLeast(0)).coerceAtLeast(0)
            for (i in skip until scrollback.size) out.add(rendered(scrollback[i]))
        }
        // On the shell screen only go down to the last non-blank row (or the
        // cursor): trailing blank rows would push the prompt off the top when the
        // view follows the bottom. A full-screen app owns every row.
        var lastRow = if (inAlt) rows - 1 else cr
        if (!inAlt) {
            for (r in rows - 1 downTo cr + 1) { if (rowNonBlank(screen[r])) { lastRow = r; break } }
        }
        for (r in 0..lastRow) {
            out.add(if (r == cr && cursorVisible) buildLine(screen[r], cc) else rendered(screen[r]))
        }
        return out
    }

    /** The same content as [snapshot], joined into one string (tests, copy). */
    fun render(scrollbackLimit: Int = DEFAULT_RENDER_SCROLLBACK): AnnotatedString = buildAnnotatedString {
        snapshot(scrollbackLimit).forEachIndexed { index, line ->
            if (index > 0) append('\n')
            append(line)
        }
    }

    private fun rendered(line: Line): AnnotatedString =
        line.text ?: buildLine(line, -1).also { line.text = it }

    private fun buildLine(line: Line, cursorCol: Int): AnnotatedString = buildAnnotatedString {
        val cells = line.cells
        var last = -1
        for (i in cells.lastIndex downTo 0) { if (!isBlank(cells[i])) { last = i; break } }
        val end = minOf(maxOf(last, cursorCol), cells.lastIndex)
        var i = 0
        while (i <= end) {
            val cell = cells[i]
            val from = length
            if (i == cursorCol) {
                append(cell.ch)
                addStyle(CURSOR_STYLE, from, length)
                i++
                continue
            }
            while (i <= end && i != cursorCol && sameStyle(cells[i], cell)) { append(cells[i].ch); i++ }
            styleOf(cell)?.let { addStyle(it, from, length) }
        }
    }

    private fun sameStyle(a: Cell, b: Cell) = a.fg == b.fg && a.bg == b.bg && a.flags == b.flags

    /** Null for default-styled text, which needs no span at all. */
    private fun styleOf(cell: Cell): SpanStyle? {
        var fg = cell.fg
        var bg = cell.bg
        if (cell.flags and INVERSE != 0) {
            val swapped = fg
            fg = if (bg == BG_NONE) TERM_BG else bg
            bg = swapped
        }
        if (fg == FG && bg == BG_NONE && cell.flags and (BOLD or DIM or ITALIC or UNDERLINE) == 0) return null
        val color = Color(fg).let { if (cell.flags and DIM != 0) it.copy(alpha = 0.6f) else it }
        return SpanStyle(
            color = color,
            background = if (bg == BG_NONE) Color.Unspecified else Color(bg),
            fontWeight = if (cell.flags and BOLD != 0) FontWeight.Bold else null,
            fontStyle = if (cell.flags and ITALIC != 0) FontStyle.Italic else null,
            textDecoration = if (cell.flags and UNDERLINE != 0) TextDecoration.Underline else null,
        )
    }

    private fun isBlank(c: Cell) = c.ch == ' ' && c.bg == BG_NONE && c.flags and (INVERSE or UNDERLINE) == 0
    private fun rowNonBlank(row: Line) = row.cells.any { !isBlank(it) }

    // ── Grid helpers ──────────────────────────────────────────────────────────

    private fun newLine(width: Int = cols) = Line(Array(width) { BLANK })
    private fun blankGrid() = Array(rows) { newLine() }

    companion object {
        private const val NORMAL = 0
        private const val ESC = 1
        private const val CSI = 2
        private const val STRING = 3
        private const val STRING_ESC = 4
        private const val CHARSET = 5
        private const val SKIP = 6
        private const val DEFAULT_RENDER_SCROLLBACK = 200
        private const val MAX_PARAM = 9999
        private const val MAX_PARAM_CHARS = 128

        private const val BOLD = 1
        private const val DIM = 2
        private const val ITALIC = 4
        private const val UNDERLINE = 8
        private const val INVERSE = 16

        private const val ESCC = '\u001B'
        private const val BEL = '\u0007'

        private const val FG = 0xFFD7DCE5L
        private const val TERM_BG = 0xFF0B0E14L
        private const val BG_NONE = 0L // sentinel: "no explicit background"
        private val CURSOR_STYLE = SpanStyle(color = Color(TERM_BG), background = Color(0xFF6BE58A))

        // Cells are immutable, so every empty cell can be this one instance.
        private val BLANK = Cell(' ', FG, BG_NONE, 0)

        // DEC Special Graphics (ESC ( 0), for '`'..'~': the box-drawing set that
        // ncurses apps (mc, dialog, htop) use for borders.
        private const val ACS = "◆▒␉␌␍␊°±␤␋┘┐┌└┼⎺⎻─⎼⎽├┤┴┬│≤≥π≠£·"

        private val ANSI16 = longArrayOf(
            0xFF1B1D23, 0xFFE05561, 0xFF8CC265, 0xFFD18F52,
            0xFF4AA5F0, 0xFFC162DE, 0xFF42B3C2, 0xFFD7DCE5,
            0xFF5C6370, 0xFFFF616E, 0xFFA5E075, 0xFFF0A45D,
            0xFF4DC4FF, 0xFFDE73FF, 0xFF4CD1E0, 0xFFFFFFFF,
        )

        private fun xterm256(index: Int): Long {
            val n = index.coerceIn(0, 255)
            return when {
                n < 16 -> ANSI16[n]
                n <= 231 -> {
                    val v = n - 16
                    val r = v / 36; val g = (v % 36) / 6; val b = v % 6
                    fun s(x: Int) = if (x == 0) 0L else 55L + x * 40
                    0xFF000000L or (s(r) shl 16) or (s(g) shl 8) or s(b)
                }
                else -> { val g = (8 + (n - 232) * 10).toLong(); 0xFF000000L or (g shl 16) or (g shl 8) or g }
            }
        }
    }
}
