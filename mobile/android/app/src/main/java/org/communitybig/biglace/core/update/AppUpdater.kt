package org.communitybig.biglace.core.update

import android.content.Context
import android.content.Intent
import android.os.Build
import android.provider.Settings
import androidx.core.content.FileProvider
import androidx.core.net.toUri
import androidx.core.content.pm.PackageInfoCompat
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import org.communitybig.biglace.R
import org.communitybig.biglace.core.data.SettingsStore
import org.json.JSONArray
import java.io.ByteArrayOutputStream
import java.io.File
import java.io.IOException
import java.io.InputStream
import java.net.HttpURLConnection
import java.net.URL
import java.security.MessageDigest

/** A newer build published on GitHub, for this device's CPU architecture. */
data class AppRelease(
    val versionCode: Long,
    val versionName: String,
    val notes: String,
    val apkUrl: String,
    val sizeBytes: Long,
    /** Hex SHA-256 of the APK, when GitHub (or a `.sha256` asset) provides one. */
    val sha256: String,
)

sealed interface UpdateState {
    data object Idle : UpdateState
    data object Checking : UpdateState
    data class UpToDate(val versionName: String) : UpdateState
    data class Available(val release: AppRelease) : UpdateState

    /** [progress] is 0..1, or -1 when the size is unknown. */
    data class Downloading(val release: AppRelease, val progress: Float) : UpdateState
    data class ReadyToInstall(val release: AppRelease, val apk: File) : UpdateState

    /** [release] is null when the check itself failed. */
    data class Failed(val release: AppRelease?, val message: String) : UpdateState
}

/** `biglace-mobile-<versionName>-vc<versionCode>-<abi>.apk`, as published by mobile/dist. */
internal data class ApkAssetName(val versionName: String, val versionCode: Long, val abi: String)

private val APK_ASSET = Regex("""^biglace-mobile-(.+)-vc(\d+)-([a-z0-9_-]+)\.apk$""")

internal fun parseApkAssetName(name: String): ApkAssetName? =
    APK_ASSET.matchEntire(name)?.let { m ->
        val code = m.groupValues[2].toLongOrNull() ?: return null
        ApkAssetName(m.groupValues[1], code, m.groupValues[3])
    }

/** Whether enough time passed since [lastCheckMs] for another automatic check. */
internal fun isCheckDue(nowMs: Long, lastCheckMs: Long, intervalMs: Long = AUTO_CHECK_INTERVAL_MS): Boolean =
    lastCheckMs <= 0 || nowMs - lastCheckMs >= intervalMs || nowMs < lastCheckMs

/** Automatic checks are spaced by this much (GitHub allows 60 anonymous API calls per hour). */
internal const val AUTO_CHECK_INTERVAL_MS = 30 * 60 * 1000L

/** Only GitHub release downloads of this repository are acceptable APK sources. */
internal fun isTrustedApkUrl(url: String): Boolean = runCatching {
    val uri = java.net.URI(url)
    uri.scheme == "https" && uri.host == "github.com" && uri.rawPath.startsWith("/$REPO/releases/download/")
}.getOrDefault(false)

private const val REPO = "big-comm/biglace"

/**
 * In-app updates from GitHub Releases. The release pipeline attaches one APK
 * per ABI to the same release as the Linux package and the Windows MSI
 * (.github/workflows/android.yml); APKs are recognised by their file name, so
 * any other asset is ignored. The app downloads the APK for this device and
 * hands it to the system installer, which only accepts it if it is signed with
 * the same key as the installed app.
 */
class AppUpdater(
    context: Context,
    private val settings: SettingsStore,
    private val scope: CoroutineScope,
) {
    private val appContext = context.applicationContext

    private val _state = MutableStateFlow<UpdateState>(UpdateState.Idle)
    val state: StateFlow<UpdateState> = _state.asStateFlow()

    private var job: Job? = null

    val installedVersionCode: Long by lazy {
        PackageInfoCompat.getLongVersionCode(appContext.packageManager.getPackageInfo(appContext.packageName, 0))
    }
    val installedVersionName: String by lazy {
        appContext.packageManager.getPackageInfo(appContext.packageName, 0).versionName.orEmpty()
    }

    /**
     * Quiet background check whenever the app comes to the foreground, at most
     * every [AUTO_CHECK_INTERVAL_MS]; failures stay silent. It used to run only
     * when the screen was created, with a 6 h gap — so a release published
     * shortly after one check went unnoticed for hours, and reopening the app
     * (whose process the mesh service keeps alive) never checked at all.
     */
    fun checkIfDue() {
        // An offer already on screen (or a download in flight) must not be
        // replaced by a fresh check — that would discard a finished download.
        when (_state.value) {
            is UpdateState.Available, is UpdateState.Downloading, is UpdateState.ReadyToInstall -> return
            else -> Unit
        }
        if (!isCheckDue(System.currentTimeMillis(), settings.lastUpdateCheck)) return
        scope.launch { withContext(Dispatchers.IO) { cleanDownloads(keepCode = Long.MAX_VALUE) } }
        check(manual = false)
    }

    /** [manual]: the user asked, so report "up to date" and errors, and ignore "Not now". */
    fun check(manual: Boolean) {
        if (job?.isActive == true) return
        if (manual) _state.value = UpdateState.Checking
        job = scope.launch {
            try {
                val release = withContext(Dispatchers.IO) { fetchLatest() }
                settings.lastUpdateCheck = System.currentTimeMillis()
                _state.value = when {
                    release == null -> if (manual) UpdateState.UpToDate(installedVersionName) else UpdateState.Idle
                    !manual && release.versionCode <= settings.skippedUpdateCode -> UpdateState.Idle
                    else -> UpdateState.Available(release)
                }
            } catch (e: Exception) {
                _state.value = if (manual) {
                    UpdateState.Failed(null, text(R.string.update_check_failed, reason(e)))
                } else {
                    UpdateState.Idle
                }
            }
        }
    }

    /** "Not now": don't offer this version again automatically. */
    fun postpone() {
        val current = _state.value
        val release = when (current) {
            is UpdateState.Available -> current.release
            is UpdateState.Failed -> current.release
            is UpdateState.ReadyToInstall -> current.release
            else -> null
        }
        if (current is UpdateState.Downloading) return
        release?.let { settings.skippedUpdateCode = it.versionCode }
        _state.value = UpdateState.Idle
    }

    fun dismissStatus() {
        val current = _state.value
        if (current is UpdateState.UpToDate || (current is UpdateState.Failed && current.release == null)) {
            _state.value = UpdateState.Idle
        }
    }

    fun download(release: AppRelease) {
        if (job?.isActive == true) return
        _state.value = UpdateState.Downloading(release, 0f)
        job = scope.launch {
            try {
                val apk = withContext(Dispatchers.IO) {
                    downloadApk(release) { _state.value = UpdateState.Downloading(release, it) }
                }
                _state.value = UpdateState.ReadyToInstall(release, apk)
                install(apk)
            } catch (e: Exception) {
                _state.value = UpdateState.Failed(release, text(R.string.update_download_failed, reason(e)))
            }
        }
    }

    /** Android asks the user, per app, before it may install APKs. */
    fun canInstall(): Boolean = appContext.packageManager.canRequestPackageInstalls()

    fun openInstallPermission() {
        runCatching {
            appContext.startActivity(
                Intent(Settings.ACTION_MANAGE_UNKNOWN_APP_SOURCES, "package:${appContext.packageName}".toUri())
                    .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
            )
        }
    }

    /** Hand the APK to the system installer. */
    fun install(apk: File): Boolean = runCatching {
        val uri = FileProvider.getUriForFile(appContext, "${appContext.packageName}.fileprovider", apk)
        appContext.startActivity(
            Intent(Intent.ACTION_VIEW)
                .setDataAndType(uri, "application/vnd.android.package-archive")
                .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_ACTIVITY_NEW_TASK),
        )
    }.isSuccess

    // ── GitHub ───────────────────────────────────────────────────────────────

    /** The newest Android release for this device's ABI, if newer than the installed app. */
    private fun fetchLatest(): AppRelease? {
        val releases = JSONArray(httpGetText("https://api.github.com/repos/$REPO/releases?per_page=30"))
        val abis = Build.SUPPORTED_ABIS.toList()
        var best: AppRelease? = null
        var bestAbiRank = Int.MAX_VALUE
        for (i in 0 until releases.length()) {
            val rel = releases.optJSONObject(i) ?: continue
            if (rel.optBoolean("draft") || rel.optBoolean("prerelease")) continue
            val assets = rel.optJSONArray("assets") ?: continue
            val byName = (0 until assets.length()).mapNotNull { assets.optJSONObject(it) }
                .associateBy { it.optString("name") }
            for ((name, asset) in byName) {
                val parsed = parseApkAssetName(name) ?: continue
                val rank = abis.indexOf(parsed.abi).takeIf { it >= 0 } ?: continue
                val url = asset.optString("browser_download_url")
                if (!isTrustedApkUrl(url)) continue
                val current = best
                val better = current == null || parsed.versionCode > current.versionCode ||
                    (parsed.versionCode == current.versionCode && rank < bestAbiRank)
                if (!better) continue
                val digest = asset.optString("digest").removePrefix("sha256:")
                    .takeIf { asset.optString("digest").startsWith("sha256:") }
                best = AppRelease(
                    versionCode = parsed.versionCode,
                    versionName = parsed.versionName,
                    notes = rel.optString("body").trim(),
                    apkUrl = url,
                    sizeBytes = asset.optLong("size", 0L),
                    sha256 = digest ?: byName["$name.sha256"]?.optString("browser_download_url")
                        ?.takeIf(::isTrustedApkUrl)
                        ?.let { runCatching { httpGetText(it).trim().substringBefore(' ') }.getOrNull() }
                        .orEmpty(),
                )
                bestAbiRank = rank
            }
        }
        return best?.takeIf { it.versionCode > installedVersionCode }
    }

    private fun downloadApk(release: AppRelease, onProgress: (Float) -> Unit): File {
        check(isTrustedApkUrl(release.apkUrl)) { "Untrusted download URL" }
        val dir = File(appContext.cacheDir, UPDATES_DIR).apply { mkdirs() }
        cleanDownloads(keepCode = release.versionCode)
        val out = File(dir, "biglace-${release.versionCode}.apk")
        val conn = open(release.apkUrl)
        try {
            if (conn.responseCode !in 200..299) throw IOException("HTTP ${conn.responseCode}")
            val total = release.sizeBytes.takeIf { it > 0 } ?: conn.contentLengthLong
            val digest = MessageDigest.getInstance("SHA-256")
            var read = 0L
            var lastPercent = -1
            conn.inputStream.use { input ->
                out.outputStream().use { output ->
                    val buf = ByteArray(64 * 1024)
                    while (true) {
                        val n = input.read(buf)
                        if (n < 0) break
                        output.write(buf, 0, n)
                        digest.update(buf, 0, n)
                        read += n
                        if (total > 0) {
                            val percent = (read * 100 / total).toInt()
                            if (percent != lastPercent) {
                                lastPercent = percent
                                onProgress(read.toFloat() / total)
                            }
                        } else if (lastPercent != -2) {
                            lastPercent = -2
                            onProgress(-1f)
                        }
                    }
                }
            }
            val hash = digest.digest().joinToString("") { "%02x".format(it) }
            val complete = total <= 0 || read == total
            if (!complete || (release.sha256.isNotBlank() && !hash.equals(release.sha256, ignoreCase = true))) {
                out.delete()
                throw IOException(text(R.string.update_verify_failed))
            }
        } catch (e: Exception) {
            out.delete()
            throw e
        } finally {
            conn.disconnect()
        }
        return out
    }

    /** Drop downloaded APKs other than [keepCode] (and any already installed). */
    private fun cleanDownloads(keepCode: Long) {
        File(appContext.cacheDir, UPDATES_DIR).listFiles()?.forEach { file ->
            val code = file.name.removePrefix("biglace-").removeSuffix(".apk").toLongOrNull()
            if (code == null || code != keepCode || code <= installedVersionCode) file.delete()
        }
    }

    private fun open(url: String): HttpURLConnection =
        (URL(url).openConnection() as HttpURLConnection).apply {
            connectTimeout = 15_000
            readTimeout = 60_000
            // GitHub serves release assets through a redirect to its CDN.
            instanceFollowRedirects = true
            setRequestProperty("Accept", "application/vnd.github+json, application/octet-stream")
            setRequestProperty("User-Agent", "BigLace-Android/$installedVersionName")
        }

    private fun httpGetText(url: String): String {
        val conn = open(url)
        try {
            if (conn.responseCode !in 200..299) throw IOException("HTTP ${conn.responseCode}")
            return readLimited(conn.inputStream)
        } finally {
            conn.disconnect()
        }
    }

    private fun readLimited(input: InputStream): String = input.use { stream ->
        val out = ByteArrayOutputStream()
        val buffer = ByteArray(16 * 1024)
        while (true) {
            val n = stream.read(buffer)
            if (n < 0) break
            out.write(buffer, 0, n)
            if (out.size() > MAX_API_BYTES) throw IOException("Response too large")
        }
        out.toString(Charsets.UTF_8.name())
    }

    private fun reason(e: Exception): String =
        e.message?.takeIf { it.isNotBlank() } ?: e.javaClass.simpleName

    private fun text(id: Int, vararg args: Any): String = appContext.getString(id, *args)

    private companion object {
        const val UPDATES_DIR = "updates"
        const val MAX_API_BYTES = 4 * 1024 * 1024
    }
}
