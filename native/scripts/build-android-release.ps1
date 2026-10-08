param(
    [switch]$DeployAndroid
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

if ($env:OS -ne "Windows_NT") {
    throw "This release build script must run on Windows."
}

$nativeDir = Split-Path -Parent $PSScriptRoot
$repoRoot = Split-Path -Parent $nativeDir
$androidVersionName = "0.1.0"
# This is deliberately stable for development release builds. Android permits
# reinstalling the same versionCode with `adb install -r`; change it only when
# deliberately versioning a newer installable release.
$androidVersionCode = 16777474
$androidPackage = "app.sanctuaryplayer.android"
$ndkVersion = "30.0.16248370"
$buildToolsVersion = "36.0.0"

function Require-File([string]$Path, [string]$Description) {
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "$Description not found: $Path"
    }
}

function Run([string]$Executable, [string[]]$Arguments, [string]$WorkingDirectory = $nativeDir) {
    Write-Host "> $Executable $($Arguments -join ' ')"
    Push-Location $WorkingDirectory
    try {
        & $Executable @Arguments
        if ($LASTEXITCODE -ne 0) {
            throw "Command failed with exit code ${LASTEXITCODE}: $Executable"
        }
    }
    finally {
        Pop-Location
    }
}

    $sdk = if ($env:ANDROID_HOME) { $env:ANDROID_HOME } else { Join-Path $env:LOCALAPPDATA "Android\Sdk" }
    $ndk = if ($env:ANDROID_NDK_ROOT) { $env:ANDROID_NDK_ROOT } else { Join-Path $sdk "ndk\$ndkVersion" }
    $buildTools = Join-Path $sdk "build-tools\$buildToolsVersion"
    $toolchain = Join-Path $ndk "toolchains\llvm\prebuilt\windows-x86_64\bin"
    $linker = Join-Path $toolchain "aarch64-linux-android24-clang.cmd"
    $strip = Join-Path $toolchain "llvm-strip.exe"
    $zipalign = Join-Path $buildTools "zipalign.exe"
    $apksigner = Join-Path $buildTools "apksigner.bat"
    $aapt = Join-Path $buildTools "aapt.exe"
    $gradle = Join-Path $nativeDir "android-gradle\gradlew.bat"
    $debugKeystore = Join-Path $env:USERPROFILE ".android\debug.keystore"

    Require-File $linker "Android NDK linker"
    Require-File $strip "Android NDK strip tool"
    Require-File $zipalign "zipalign"
    Require-File $apksigner "apksigner"
    Require-File $aapt "aapt"
    Require-File $gradle "Gradle wrapper"
    Require-File $debugKeystore "Android development signing keystore"

    $env:ANDROID_HOME = $sdk
    $env:ANDROID_SDK_ROOT = $sdk
    $env:ANDROID_NDK_ROOT = $ndk
    $env:CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER = $linker
    Set-Item -Path Env:CC_aarch64_linux_android -Value $linker
    Set-Item -Path Env:CXX_aarch64_linux_android -Value (Join-Path $toolchain "aarch64-linux-android24-clang++.cmd")
    Set-Item -Path Env:AR_aarch64_linux_android -Value (Join-Path $toolchain "llvm-ar.exe")

    if (-not $env:JAVA_HOME) {
        $defaultJava = "C:\Program Files\Microsoft\jdk-17.0.20.101-hotspot"
        if (Test-Path -LiteralPath $defaultJava -PathType Container) {
            $env:JAVA_HOME = $defaultJava
        }
    }

    Write-Host "Building optimized Android native library..."
    $cargoTarget = Join-Path $nativeDir "target\android-cargo"
    Run "cargo" @(
        "build", "--release",
        "-p", "sanctuary-player-android",
        "--lib",
        "--target", "aarch64-linux-android",
        "--target-dir", $cargoTarget
    )

    $nativeLibrary = Join-Path $cargoTarget "aarch64-linux-android\release\libsanctuary_player_android.so"
    Require-File $nativeLibrary "Android release native library"
    $jniDir = Join-Path $nativeDir "target\android-jniLibs\arm64-v8a"
    New-Item -ItemType Directory -Force -Path $jniDir | Out-Null
    $jniLibrary = Join-Path $jniDir "libsanctuary_player_android.so"
    Run $strip @("--strip-debug", "-o", $jniLibrary, $nativeLibrary)

    Write-Host "Packaging Android release APK..."
    Run $gradle @(
        "--no-daemon",
        "-PsanctuaryVersionName=$androidVersionName",
        "-PsanctuaryVersionCode=$androidVersionCode",
        ":app:assembleRelease"
    ) (Join-Path $nativeDir "android-gradle")

    $unsignedApk = Join-Path $nativeDir "android-gradle\app\build\outputs\apk\release\app-release-unsigned.apk"
    Require-File $unsignedApk "Gradle unsigned release APK"
    $apkDir = Join-Path $nativeDir "target\release\apk"
    New-Item -ItemType Directory -Force -Path $apkDir | Out-Null
    $alignedApk = Join-Path $nativeDir "target\android-sanctuary-release-aligned.apk"
    $finalApk = Join-Path $apkDir "sanctuary_player_android.apk"
    Remove-Item -LiteralPath $alignedApk, $finalApk, "$finalApk.idsig" -Force -ErrorAction SilentlyContinue

    Run $zipalign @("-f", "4", $unsignedApk, $alignedApk)
    Run $apksigner @(
        "sign",
        "--ks", $debugKeystore,
        "--ks-pass", "pass:android",
        "--key-pass", "pass:android",
        "--out", $finalApk,
        $alignedApk
    )
    Remove-Item -LiteralPath $alignedApk -Force
    Run $zipalign @("-c", "4", $finalApk)
    Run $apksigner @("verify", "--verbose", "--print-certs", $finalApk)
    $badging = & $aapt dump badging $finalApk
    if ($LASTEXITCODE -ne 0) {
        throw "aapt failed to inspect the release APK."
    }
    $packageLine = $badging | Select-Object -First 1
    if ($packageLine -notmatch "versionCode='$androidVersionCode'" -or
        $packageLine -notmatch "versionName='$([regex]::Escape($androidVersionName))'") {
        throw "Release APK version did not match the configured version: $packageLine"
    }
    Write-Host $packageLine
    Write-Host "Android release APK: $finalApk"

    if ($DeployAndroid) {
        $adbConnect = Join-Path $env:LOCALAPPDATA "bin\adb-connect-phone.cmd"
        $adb = Join-Path $sdk "platform-tools\adb.exe"
        Require-File $adbConnect "adb-connect-phone helper"
        Require-File $adb "ADB"
        Run $adbConnect @() $repoRoot
        Run $adb @("install", "-r", $finalApk) $repoRoot
        Write-Host "Installed $androidPackage versionCode=$androidVersionCode on the connected phone."
    }

