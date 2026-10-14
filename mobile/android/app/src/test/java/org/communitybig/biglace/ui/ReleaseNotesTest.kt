package org.communitybig.biglace.ui

import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.LinkAnnotation
import androidx.compose.ui.text.font.FontWeight
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class ReleaseNotesTest {
    // What build-package publishes today, verbatim.
    private val body = """
        - **Date and Time    :** 2026-10-14 18:35:07
        - **PKG File         :** biglace-26.10.14-1838-x86_64.pkg.tar.zst
        - **SIG File         :** biglace-26.10.14-1838-x86_64.pkg.tar.zst.sig
        - **MD5 File         :** biglace-26.10.14-1838-x86_64.pkg.tar.zst.md5
        - **Repository Name  :** big-comm/biglace


        **Full Changelog**: https://github.com/big-comm/biglace/compare/26.10.14-1806...26.10.14-1835
    """.trimIndent()

    @Test
    fun packageBuildDetailsAreDropped() {
        val cleaned = cleanReleaseNotes(body)
        assertFalse(cleaned.contains("pkg.tar.zst"))
        assertFalse(cleaned.contains("Repository Name"))
        assertTrue(cleaned.startsWith("**Full Changelog**"))
    }

    @Test
    fun markdownIsRenderedInsteadOfShownRaw() {
        val text = renderReleaseNotes(cleanReleaseNotes(body), Color.Blue)
        val url = "https://github.com/big-comm/biglace/compare/26.10.14-1806...26.10.14-1835"
        assertEquals("Full Changelog: $url", text.text)
        assertFalse(text.text.contains("**"))

        val bold = text.spanStyles.single { it.item.fontWeight == FontWeight.Bold }
        assertEquals("Full Changelog", text.text.substring(bold.start, bold.end))

        val link = text.getLinkAnnotations(0, text.length).single()
        assertEquals(url, (link.item as LinkAnnotation.Url).url)
        assertEquals(url, text.text.substring(link.start, link.end))
    }

    @Test
    fun bulletsHeadingsAndNamedLinks() {
        val text = renderReleaseNotes(
            "## What's new\n- Faster terminal\n* See [the docs](https://example.org/docs).",
            Color.Blue,
        )
        assertEquals("What's new\n• Faster terminal\n• See the docs.", text.text)
        val link = text.getLinkAnnotations(0, text.length).single()
        assertEquals("https://example.org/docs", (link.item as LinkAnnotation.Url).url)
    }

    @Test
    fun notesWithOnlyPackageDetailsLeaveNothingToShow() {
        assertTrue(cleanReleaseNotes("- **PKG File :** x.pkg.tar.zst\n- **MD5 File :** x.md5").isBlank())
    }
}
