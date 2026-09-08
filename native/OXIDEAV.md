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
`ac54031`, `981e3ae`). Those commits are useful reference implementations, not
application dependencies.

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
When reproducing the external-memory import, also mirror `981e3ae`: Vulkan uses
`VkMemoryDedicatedAllocateInfo` for the shared image, so the GL memory object must
be marked `GL_DEDICATED_MEMORY_OBJECT_EXT` before `glImportMemoryFdEXT`. NVIDIA may
otherwise accept the import while framebuffer writes remain invisible.

The corrected post-`981e3ae` reference-player benchmark used the local 10.03 s,
1280x720/60 fps Twitch segment (600 frames, five muted paced runs per path).
Compared with VDPAU decode followed by CPU `materialize()` and wgpu upload, the
four-slot async bridge reduced mean total process CPU time from 7.696 s to 2.752 s
(**64.2% less**, 12.83 to 4.59 ms/frame), while peak process RSS rose only from
about 235.9 to 239.1 MiB. Earlier measurements made while the external-memory
target rendered black are superseded by these post-fix numbers. Treat these as a
GhostBSD/GTX-1080 reference result, not a portable performance guarantee.

## Twitch VOD manifest acquisition

Twitch VOD source extraction is SanctuaryPlayer-owned rather than an OxideAV
framework concern. `native/app/src/twitch.rs` resolves a numeric Twitch VOD ID to
its signed HLS master-playlist URL without invoking yt-dlp.

The resolver deliberately implements only the playback flow Sanctuary needs:

1. POST one GraphQL query to `gql.twitch.tv` for
   `videoPlaybackAccessToken { value signature }`.
2. Construct the signed `usher.ttvnw.net/vod/<id>.m3u8` URL locally.
3. Advertise only H.264 in `supported_codecs`, matching the codecs currently
   available to the native player.

There is no separate Twitch metadata request and the resolver does not fetch the
master playlist itself. `AppCommand::OpenVideo` now dispatches Twitch VOD resolution
onto a worker thread, keeps the winit event thread responsive, and polls the result
through normal app updates. For the current bring-up stage, success is shown in a
copyable `Twitch HLS URL` dialog and resolver failures are shown as errors; recognised
YouTube inputs are rejected with an explicit currently-unsupported dialog.

The next media-session step is to replace the success dialog with an OxideAV HLS
open. At that boundary, convert the returned ordinary `https://...m3u8` URL to
OxideAV's `hls+https://...` source URI; the OxideAV HLS source then performs the
master-playlist GET.

This depends on Twitch's web-player GraphQL/Usher protocol rather than a stable
public playback API, so all Twitch-specific request shape, client ID and token
handling remain isolated in that module for straightforward future replacement.

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
