# SanctuaryPlayer repository notes

## Android debuggability is intentional

- Keep `android:debuggable="true"` in `native/android-gradle/app/src/main/AndroidManifest.xml`, including for release APKs.
- This project deliberately keeps Android release builds debuggable so ADB/log retrieval and on-device diagnostics remain available during development and testing.
- Do not remove the attribute merely to satisfy Android Lint's `HardcodedDebugMode` warning.
- If release lint rejects this setting, suppress `HardcodedDebugMode` explicitly in the Gradle Android lint configuration rather than changing the manifest.
- A release APK here means release/optimized native code; it is still intentionally debuggable unless the user explicitly asks to change that policy.
