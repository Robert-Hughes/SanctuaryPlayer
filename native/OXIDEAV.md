# OxideAV native integration

This document records how the native application consumes the locally developed
OxideAV stack. Framework implementation details, measurements and generic follow-ups
live in the corresponding OxideAV checkout's root `LOCAL-CHANGES.md`; this file
contains only SanctuaryPlayer-specific integration decisions and work.

## Dependency boundary

SanctuaryPlayer consumes OxideAV **library crates directly**. It must not depend on
`oxideplay`; that binary is a reference consumer used to exercise OxideAV features.
The native app owns its own winit event loop, wgpu device/queue/surface, egui pass,
playback state and media-session scheduling.

The native workspace keeps normal versioned dependencies in `native/Cargo.toml`:

```toml
oxideav = "0.0.3"
oxideav-meta = { version = "0.0.1", default-features = false, features = [
    "aac", "h264", "mp4", "mpegts", "source", "http", "hls", "vdpau",
] }
oxideav-pixfmt = "0.1"
oxideav-audio-filter = "0.1"
oxideav-sysaudio = "0.1"
```

Local co-development uses `[patch.crates-io]` entries pointing at
`../../oxideav/crates/...`. Patch every OxideAV crate in the selected
dependency graph: mixing local and crates.io copies can create duplicate core
traits/types with incompatible Rust identities.

## OxideAV capabilities currently available to the app

The local OxideAV workspace provides the pieces needed for native playback:

- FreeBSD OSS audio in `oxideav-sysaudio` (`5195ab8`).
- HLS VOD source support with lazy MPEG-TS segment access (`147e0b6`, `65e03ab`,
  `7f0acec`).
- Shared H.264 streaming frontends and software picture state (`1041a0f`,
  `8d2a3e5`).
- FreeBSD VDPAU H.264 streaming decode (`1abfbe8`) with explicit unsupported-case
  fallback rather than silent approximation (`6417b12`).
- Retainable decoded-frame ownership through `FrameLease` (`c6e6f02`, `4c7099a`).
- Native pooled software-H.264 arena output (`46f8433`, `94b6372`, `fda3143`).
- Retainable VDPAU hardware-surface leases (`67ca9af`, `c0e23db`).
- Native AAC fast enough for real-time playback without Symphonia (`38a8443`,
  `26f4127`, `b760012`).
- Decoder output parameters for late-discovered output shape (`72ef547`, `a0c9d78`).

`oxideplay` also demonstrates lease retention through a player queue, direct
arena-backed YUV420P upload to wgpu (`ad91b3c`, `1a0f621`, `4d0350c`), and the
FreeBSD/NVIDIA zero-CPU-copy hardware path from a retained VDPAU surface through
GLX interop into the existing wgpu/Vulkan renderer (`23a415e`, `07d07e9`,
`ac54031`). Those commits are useful reference implementations, not application
dependencies.

## Native media boundary

The intended ownership split is:

```text
SanctuaryPlayer UI / state
        ↓
Sanctuary media-session abstraction
        ↓
OxideAV source / demux / codec / audio crates
        ↓
decoded FrameLease + audio frames
        ↓
Sanctuary-owned renderer / audio sink / playback clock
```

The media-session sink should retain `FrameLease`s directly. For software H.264,
`FrameLease::ArenaVideo` can remain on the same pooled CPU allocation from decoder
reconstruction, through queues, to the wgpu upload call. The app should implement
the same lease-aware idea as the `oxideplay` reference path: validate native
YUV420P arena geometry/strides and pass the borrowed plane slices directly to
`wgpu::Queue::write_texture()` without `materialize()` or a plane repack. If that
logic grows beyond a small adapter, prefer extracting a reusable OxideAV wgpu helper
rather than depending on `oxideplay`.

For hardware H.264, retain the `HardwareVideo` lease until GPU work has finished
reading the surface. The reference player now proves a Vulkan-preserving
zero-CPU-copy route on the GTX 1080/NVIDIA stack: `GL_NV_vdpau_interop2` exposes
full-frame Y plus interleaved UV textures, a GL shader converts them to RGBA in
Vulkan-exported external memory, and a raw Vulkan GPU copy moves that image into a
normal wgpu-owned texture. The current player uses four independent in-flight
bridge slots; each slot retains its hardware lease until a non-blocking Vulkan
fence poll proves the dependent copy finished. GL/Vulkan ordering uses GPU
semaphores and `glFlush`, with no `glFinish()` or per-frame fence wait. If all
slots are busy, the frame is dropped rather than stalling or materialising to CPU.
Sanctuary should mirror or extract that lease/slot model rather than depending on
`oxideplay`. Literal zero-copy remains a later optimisation because the GL
YUV->RGBA pass and final Vulkan image copy are still present.

## Audio integration

OxideAV AAC can discover the actual decoded sample rate/channel layout after the
container has already supplied incomplete or stale compressed-stream parameters.
`Decoder::output_params()` exposes the authoritative decoded shape and the pipeline
prefers it.

A current application integration gap is late sink configuration: the real Twitch
AAC fixture is 48 kHz, while a player can instantiate an audio sink from an earlier
44.1 kHz fallback before the first ADTS frame reveals the correct rate. Sanctuary's
native audio path should configure/reconfigure from the decoder's first real output
parameters rather than locking in the fallback.

## Local validation fixtures

The OxideAV checkout at `../../oxideav` currently carries local untracked
experiment material used during this integration:

- `sanctuary-testdata/` — preserved deterministic Big Buck Bunny MPEG-TS/H.264
  fixtures used by the reference-player and decoder regressions.
- `sanctuary-experiments/aac-native-bench/` — small native-AAC benchmark harness
  and AAC fixtures.
- `sanctuary-experiments/pes-inspect/` — small MPEG-TS/PES diagnostic utility.
- `sanctuary-experiments/twitch-2845804307/` — real-world Twitch master/media
  playlists plus one TS segment. Signed source URL files were deleted; the master
  playlist still contains captured request metadata, so keep this directory
  untracked unless it is explicitly sanitised and promoted to stable test data.
- `sanctuary-experiments/tools/yt-dlp/` — clean local upstream yt-dlp clone used by
  the wider source-extraction investigation.

Assistant-driven playback/audio tests for this work must remain muted unless sound
is explicitly authorised; use `--ao none`, null/hash sinks or equivalent.

## Current SanctuaryPlayer follow-ups

1. Build the native media-session layer directly against the OxideAV library crates.
2. Carry `FrameLease` through Sanctuary's queues and implement direct arena YUV420P
   upload in the Sanctuary-owned wgpu renderer.
3. Configure the audio sink from authoritative decoder output parameters.
4. Port or extract the proven oxideplay VDPAU/GLX/Vulkan bridge into the
   Sanctuary-owned renderer, keeping `HardwareVideo` leases GPU-resident and
   retaining CPU materialisation only as fallback.
5. Integrate HLS media-relative seeking/timeline behaviour as OxideAV gains it.
6. Extend source support for HLS discontinuities, byte ranges, fMP4/MAP, encryption,
   live reload and ABR only as real sources require them.
