# Gomobile JNI entry points are resolved by name.
-keep class go.** { *; }
-keep class community.biglace.** { *; }

# SSHJ loads crypto implementations and algorithms reflectively.
-keep class net.schmizz.sshj.** { *; }
-keep class org.bouncycastle.** { *; }
-dontwarn org.bouncycastle.**

# Optional desktop-only SSHJ authentication providers are absent on Android.
-dontwarn javax.security.auth.login.**
-dontwarn org.ietf.jgss.**
-dontwarn sun.security.x509.**
