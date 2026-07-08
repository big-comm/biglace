package org.communitybig.biglace.core.ssh

/** Which SSH auth methods to attempt, chosen by the user before connecting. */
enum class AuthMode { AUTO, KEY, PASSWORD }

/** Result of a connection/auth probe (the "Test" button), for the result dialog. */
data class ProbeResult(val ok: Boolean, val message: String)
