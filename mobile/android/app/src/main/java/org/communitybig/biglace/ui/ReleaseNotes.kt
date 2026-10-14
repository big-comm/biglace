package org.communitybig.biglace.ui

import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.LinkAnnotation
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.TextLinkStyles
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextDecoration
import androidx.compose.ui.text.withLink
import androidx.compose.ui.text.withStyle

/**
 * Lines the release pipeline (BigCommunity build-package) writes about the
 * Linux package — `- **PKG File :** biglace-….pkg.tar.zst` and friends. They
 * mean nothing to someone updating the phone app, so they are left out.
 */
private val BUILD_METADATA = Regex(
    """^\s*[-*]\s*\*\*\s*(Date and Time|PKG File|SIG File|MD5 File|Repository Name)\b.*$""",
    RegexOption.IGNORE_CASE,
)

private val INLINE = Regex("""\*\*(.+?)\*\*|`([^`]+)`|\[([^\]]+)]\((https?://[^)\s]+)\)|(https?://\S+)""")

/** Release notes without the package-build boilerplate; blank when nothing is left. */
internal fun cleanReleaseNotes(body: String): String =
    body.lines()
        .filterNot { BUILD_METADATA.matches(it) }
        .joinToString("\n")
        .replace(Regex("\n{3,}"), "\n\n")
        .trim()

/**
 * The small Markdown subset GitHub release notes use, rendered for a Text:
 * headings, bullets, **bold**, `code`, [links](url) and bare URLs (tappable).
 */
internal fun renderReleaseNotes(markdown: String, linkColor: Color): AnnotatedString {
    val linkStyles = TextLinkStyles(SpanStyle(color = linkColor, textDecoration = TextDecoration.Underline))
    return buildAnnotatedString {
        markdown.lines().forEachIndexed { index, raw ->
            if (index > 0) append('\n')
            var line = raw.trimEnd()
            val heading = Regex("""^#{1,6}\s+""").find(line)
            val bullet = Regex("""^\s*[-*+]\s+""").find(line)
            when {
                heading != null -> {
                    withStyle(SpanStyle(fontWeight = FontWeight.Bold)) {
                        appendInline(line.substring(heading.range.last + 1), linkStyles)
                    }
                    return@forEachIndexed
                }
                bullet != null -> {
                    append("• ")
                    line = line.substring(bullet.range.last + 1)
                }
            }
            appendInline(line, linkStyles)
        }
    }
}

private fun AnnotatedString.Builder.appendInline(text: String, linkStyles: TextLinkStyles) {
    var cursor = 0
    for (match in INLINE.findAll(text)) {
        append(text.substring(cursor, match.range.first))
        val (bold, code, linkText, linkUrl, bareUrl) = match.destructured
        when {
            bold.isNotEmpty() -> withStyle(SpanStyle(fontWeight = FontWeight.Bold)) {
                appendInline(bold, linkStyles)
            }
            code.isNotEmpty() -> withStyle(SpanStyle(fontFamily = FontFamily.Monospace)) { append(code) }
            linkUrl.isNotEmpty() -> withLink(LinkAnnotation.Url(linkUrl, linkStyles)) { append(linkText) }
            else -> {
                // A sentence's closing punctuation isn't part of the address.
                val url = bareUrl.trimEnd('.', ',', ')', ';', ':')
                withLink(LinkAnnotation.Url(url, linkStyles)) { append(url) }
                append(bareUrl.substring(url.length))
            }
        }
        cursor = match.range.last + 1
    }
    append(text.substring(cursor))
}
