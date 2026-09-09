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
oxideav-vdpau = "0.0.2"
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
- Native pooled software-H.264 arena output (`46f8433`, `94b6372`, `fda3143`),
  including arena-backed PAFF/SCP assembly and hard pool-exhaustion semantics
  (`d8ca4c2`).
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

The first Sanctuary media-session implementation now retains `FrameLease`s directly.
For ordinary frame-coded software H.264, `FrameLease::ArenaVideo` stays on the pooled
decoder allocation through the OxideAV sink, Sanctuary's bounded video queue, and
the renderer boundary. `d8ca4c2` also guarantees that PAFF/SCP assembly still emits
`ArenaVideo` without silently converting a ready frame to `Owned`. `94de8f2` makes
software-decoder pool pressure block in the reusable arena allocator until a retained
picture/assembly lease is released, so downstream back-pressure no longer surfaces as
an H.264 slice-level `ResourceExhausted` failure. `native/app/src/video_renderer.rs`
validates native YUV420P arena geometry/strides and passes the original borrowed
Y/U/V plane slices and their real strides directly to `wgpu::Queue::write_texture()`.
There is no `materialize()`, `VideoFrame` allocation, `plane_tight()` equivalent, or
full-frame CPU repack on the ordinary 4:2:0 path. The renderer deliberately rejects
non-arena/non-YUV420P output rather than silently copying it.

The application now exposes three explicit decode/presentation contracts through
`--decode-mode`:

- `cpu` (default) sets `CodecPreferences::no_hardware`, so software H.264 is selected
  even though VDPAU is compiled into the same runtime. Output must remain
  `FrameLease::ArenaVideo`; the renderer refuses a silent materialisation fallback.
- `vdpau-readback` sets `CodecPreferences::require_hardware` and prefers
  `h264_vdpau`. The renderer requires a VDPAU `HardwareVideo` lease, explicitly calls
  its `materialize()` implementation (`VdpVideoSurfaceGetBitsYCbCr` -> CPU I420), and
  uploads those planes through the existing wgpu YUV path. This deliberately measures
  hardware decode while retaining CPU readback before presentation.
- `vdpau-direct` uses the same required-VDPAU selection but never materialises the
  hardware frame. Sanctuary owns a port of the proven reference bridge:
  `GL_NV_vdpau_interop2` exposes full-frame Y plus interleaved UV textures, a GL
  shader converts them to RGBA in Vulkan-exported external memory, and a raw Vulkan
  GPU copy moves that image into a normal wgpu-owned texture. Four independent slots
  retain their hardware leases until a non-blocking Vulkan fence proves the dependent
  copy complete. If every slot is busy, the video frame is dropped rather than
  stalling or falling back to CPU.

The two VDPAU modes are deliberately strict: failure to obtain hardware decode or to
execute the selected presentation path is an error, not a request to silently switch
modes. `7b9a06d` supplies the generic `Executor::with_codec_preferences()` plumbing
that makes the per-session selection possible while all implementations stay
registered. The direct path remains zero-CPU-copy rather than literal zero-copy: it
still performs the GL YUV->RGBA render and one GPU-local Vulkan image copy. It also
retains the `981e3ae` dedicated-memory requirement: because Vulkan allocates the
shared image with `VkMemoryDedicatedAllocateInfo`, the GL memory object is marked
`GL_DEDICATED_MEMORY_OBJECT_EXT` before `glImportMemoryFdEXT`.

The corrected post-`981e3ae` reference-player benchmark used the local 10.03 s,
1280x720/60 fps Twitch segment (600 frames, five muted paced runs per path).
Compared with VDPAU decode followed by CPU `materialize()` and wgpu upload, the
four-slot async bridge reduced mean total process CPU time from 7.696 s to 2.752 s
(**64.2% less**, 12.83 to 4.59 ms/frame), while peak process RSS rose only from
about 235.9 to 239.1 MiB. Earlier measurements made while the external-memory
target rendered black are superseded by these post-fix numbers. Treat these as a
GhostBSD/GTX-1080 reference result, not a portable performance guarantee.

## Twitch VOD acquisition and first native playback path

Twitch VOD source extraction remains SanctuaryPlayer-owned rather than an OxideAV
framework concern. `native/app/src/twitch.rs` resolves a numeric Twitch VOD ID to
its signed HLS master-playlist URL without invoking yt-dlp:

1. POST one GraphQL query to `gql.twitch.tv` for
   `videoPlaybackAccessToken { value signature }`.
2. Construct the signed `usher.ttvnw.net/vod/<id>.m3u8` URL locally.
3. Advertise only H.264 in `supported_codecs`, matching the codecs currently
   requested by the native player.

`AppCommand::OpenVideo` now keeps both URL resolution and OxideAV source opening off
the winit event thread. After the Twitch resolver succeeds, the worker converts the
ordinary `https://...m3u8` URL to `hls+https://...`, creates a video-only OxideAV
job, and starts an `Executor` with a Sanctuary-owned `JobSink`. The job requests only
the video track, so AAC is neither decoded nor sent to an audio device in this
bring-up milestone.

The native workspace enables the `vdpau` feature alongside software H.264. Selection
is per playback session rather than compile-time: `cpu` explicitly excludes hardware,
while both VDPAU modes explicitly require it. There is therefore no priority-dependent
software/hardware fallback hidden behind the command-line choice.

The Sanctuary sink forwards each decoded `FrameLease` unchanged through a bounded
two-frame channel into a four-frame presentation queue. Playback starts paused and
keeps repainting until the first decoded frame is available, so the first picture can
be shown while paused. Once playing, Sanctuary advances its temporary video-only
wall-clock timeline only while at least one decoded video frame is buffered. If the
decoded queue runs dry, media time stalls until another frame arrives; a decoder that
cannot sustain real time therefore makes playback run slower instead of allowing the
clock to race ahead of decoded video. Due leases are still selected by PTS. The HLS
source currently chooses one rendition at
open (the existing OxideAV default is at most 720p); dynamic ABR/quality switching
is not yet implemented. The desktop launcher accepts `--video <URL-or-ID>` (or a
positional video), `--play`/`--autoplay`, and
`--decode-mode cpu|vdpau-readback|vdpau-direct`, using the same `VideoSource::parse`
rules as the in-app Change Video flow. The decode mode defaults to `cpu`.

Bounded GhostBSD validations against Twitch VOD `2845804307` exercise all three
modes from the same native binary. Each run resolved the signed HLS URL and selected
the 1280x720/60, 4,275-segment VOD:

```text
sanctuary-player --video 2845804307 --play --decode-mode cpu
sanctuary-player --video 2845804307 --play --decode-mode vdpau-readback
sanctuary-player --video 2845804307 --play --decode-mode vdpau-direct
```

The CPU run showed no VDPAU initialisation. The readback run reported the NVIDIA
580.173.02 VDPAU streaming decoder followed by Sanctuary's explicit 1280x720 CPU-I420
readback path. The direct run reported the same VDPAU decoder followed by the
four-slot `GLX interop2 -> Vulkan -> wgpu` bridge. Both hardware runs stayed alive
for the full 20-second external timeout with no playback/bridge failure; the direct
run reported no busy-slot drops. The jobs remain video-only, so no audio device was
opened during these tests. The same two hardware modes were also run for 12 seconds
against Twitch VOD `2386400830` (the stream that exposed the earlier software-path
problem); both selected 1280x720 VDPAU output with no H.264 slice-skip or bridge
errors. Its URL start-time remains unapplied until HLS seeking is wired.

This Twitch web-player GraphQL/Usher protocol is not a stable public playback API,
so all Twitch-specific request shape, client ID and token handling remain isolated
in `twitch.rs` for straightforward future replacement.

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

1. Replace the temporary video-only starvation clock with final media pacing once
   audio is present: audio-device progress should become the master clock and video
   should be scheduled/dropped against it.
2. Add the real audio path, configuring/reconfiguring the sink from the decoder's
   authoritative AAC output parameters and establishing A/V sync.
3. Wire media-relative HLS seeking (including `VideoSource::start_time`) rather than
   moving Sanctuary's presentation clock without moving the demuxer/decoder.
4. Expose useful rendition/quality selection and later ABR once the source layer can
   switch safely during playback.
5. Consider eliminating the final GPU-local image copy in `vdpau-direct` only if wgpu
   can safely own/sample the externally-written image without weakening resource-state
   correctness.
6. Extend HLS support for discontinuities, byte ranges, fMP4/MAP, encryption, live
   reload and other source shapes only as real inputs require them.
