package org.communitybig.biglace.ui.theme

import android.os.Build
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.dynamicDarkColorScheme
import androidx.compose.material3.dynamicLightColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.platform.LocalContext

private val DarkColors = darkColorScheme(
    primary = BrandBlue,
    secondary = BrandBlueLight,
    background = DarkBackground,
    surface = DarkSurface,
    onBackground = OnDark,
    onSurface = OnDark,
)

private val LightColors = lightColorScheme(
    primary = BrandBlue,
    secondary = BrandBlueLight,
    background = LightBackground,
    surface = LightSurface,
    onBackground = OnLight,
    onSurface = OnLight,
)

/**
 * App theme. Dark-first (a terminal app lives in the dark) but honours the
 * system setting, and uses Material You dynamic color on Android 12+ so the
 * accent matches the user's wallpaper; falls back to the BigLace brand blue.
 */
@Composable
fun BigLaceTheme(
    darkTheme: Boolean = isSystemInDarkTheme(),
    dynamicColor: Boolean = true,
    content: @Composable () -> Unit,
) {
    val colors = when {
        dynamicColor && Build.VERSION.SDK_INT >= Build.VERSION_CODES.S -> {
            val ctx = LocalContext.current
            if (darkTheme) dynamicDarkColorScheme(ctx) else dynamicLightColorScheme(ctx)
        }
        darkTheme -> DarkColors
        else -> LightColors
    }
    MaterialTheme(
        colorScheme = colors,
        typography = BigLaceTypography,
        content = content,
    )
}
