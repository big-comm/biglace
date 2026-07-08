package org.communitybig.biglace

import android.app.Application
import org.bouncycastle.jce.provider.BouncyCastleProvider
import java.security.Security

/** Owns the process-wide [AppContainer]. */
class BigLaceApplication : Application() {
    lateinit var container: AppContainer
        private set

    override fun onCreate() {
        super.onCreate()
        installFullBouncyCastle()
        container = AppContainer(this)
    }

    /**
     * Android ships a stripped-down BouncyCastle registered as provider "BC"
     * that lacks modern algorithms (e.g. X25519 for SSH's curve25519-sha256 key
     * exchange), so sshj fails at the transport handshake with
     * "no such algorithm: X25519 for provider BC". Replace it with the full
     * BouncyCastle bundled via sshj. Must run before any SSH connection.
     */
    private fun installFullBouncyCastle() {
        runCatching {
            Security.removeProvider("BC")
            Security.addProvider(BouncyCastleProvider())
        }
    }
}
