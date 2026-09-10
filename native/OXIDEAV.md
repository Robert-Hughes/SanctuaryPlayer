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
an H.264 slice-level `ResourceExhausted` failure. `4fe137c` adds cancellation-aware
arena waits in `oxideav-core`, `e47b459` wires the staged executor's abort token into
blocking decoders, and `3f2ce28` makes software H.264 distinguish legitimate
downstream pressure from a full pool retained entirely by decoder-owned state. An
executor stop now wakes an arena-blocked decoder even while application leases remain
checked out; a would-be self-deadlock instead fails immediately with
`ResourceExhausted` rather than sleeping forever. `native/app/src/video_renderer.rs`
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
- `vdpau-readback` prefers `h264_vdpau` and explicitly excludes `h264_sw`. This keeps
  video selection strict without imposing `require_hardware` on the AAC track in the
  same job. The renderer requires a VDPAU `HardwareVideo` lease, explicitly calls its
  `materialize()` implementation (`VdpVideoSurfaceGetBitsYCbCr` -> CPU I420), and
  uploads those planes through the existing wgpu YUV path. This deliberately measures
  hardware decode while retaining CPU readback before presentation.
- `vdpau-direct` uses the same strict H.264 selection but never materialises the
  hardware frame. Sanctuary owns a port of the proven reference bridge:
  `GL_NV_vdpau_interop2` exposes full-frame Y plus interleaved UV textures, a GL
  shader converts them to RGBA in Vulkan-exported external memory, and a raw Vulkan
  GPU copy moves that image into a normal wgpu-owned texture. Four independent slots
  retain their hardware leases until a non-blocking Vulkan fence proves the dependent
  copy complete. If every slot is busy, the video frame is dropped rather than
  stalling or falling back to CPU.

The two VDPAU modes are deliberately strict for H.264: failure to obtain VDPAU decode
or to execute the selected presentation path is an error, not a request to silently
switch the video track to software. The global `require_hardware` flag is deliberately
not used now that one job also decodes AAC; excluding `h264_sw` leaves software audio
codecs selectable. `7b9a06d` supplies the generic
`Executor::with_codec_preferences()` plumbing that makes the per-session selection
possible while all implementations stay registered. The direct path remains zero-CPU-copy rather than literal zero-copy: it
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

`AppCommand::OpenVideo` keeps both URL resolution and OxideAV source opening off the
winit event thread. After the Twitch resolver succeeds, the worker converts the
ordinary `https://...m3u8` URL to `hls+https://...`, creates one OxideAV A/V job, and
starts an `Executor` with a Sanctuary-owned `JobSink`. `@display` now requests both
the H.264 video track and the AAC audio track.

The sink forwards audio and video through one bounded two-message session channel.
Video leases remain zero-copy/retained exactly as before. Decoded `Frame::Audio`
values are converted with `oxideav-audio-filter::sample_convert::decode_to_f32`,
interleaved, and pushed into a bounded `ringbuf` SPSC queue owned by
`native/app/src/audio_output.rs`. The platform callback is supplied by
`oxideav-sysaudio`: FreeBSD/GhostBSD uses native OSS (`/dev/dsp`), Windows uses
WASAPI, macOS uses CoreAudio, and Linux uses the first working configured backend.
Sanctuary does not contain an OSS-specific device implementation.

`AudioOutput` opens the device at the decoded stream rate/channel count, keeps the
stream paused initially, and uses a 50 ms PCM preroll before it may start. If the
device negotiates a different sample rate, Sanctuary runs the existing OxideAV
polyphase `Resample` filter before f32 interleaving. A negotiated channel-count change
is currently rejected explicitly rather than silently applying an incorrect remix.
The ring is sized for roughly four seconds and the session stops draining the executor
when audio headroom falls below roughly 250 ms; the existing bounded OxideAV channels
then provide normal upstream back-pressure. During initial preroll Sanctuary permits
up to eight video leases so interleaved video messages do not prevent the small audio
preroll from filling.

Audio-device consumption is now the master media clock for A/V playback. The callback
increments `played_samples` only for real PCM popped from the ring; backend-requested
silence on underrun does **not** advance media time. The application establishes one
common media-time origin from both stream start times when available, otherwise from
the first timestamp observed on each stream, and preserves any initial A/V offset.
Video PTS values are converted onto that same timeline and due/stale frames are chosen
against the audio-derived position. Pause pauses the sysaudio stream, so the master
clock freezes naturally. For media with no audio track, the earlier video-only clock
remains as the fallback and still stalls when no decoded video is buffered.

`2fe9a28` in `oxideav-pipeline` is required for that fallback to be trustworthy. The
pipeline previously synthesised sink-facing primary streams with `start_time: Some(0)`
regardless of source metadata. Twitch MPEG-TS starts on a non-zero transport clock
(the observed VOD first segment is around 70.024 s audio / 70.060 s video), so the
fabricated zero made Sanctuary queue video around 70 s in the future while its audio
master clock started at zero. The pipeline now preserves a known source start (rescaled
when necessary) and leaves an unknown start as `None`; Sanctuary then anchors from the
first actual decoded A/V PTS values. Runtime logs print those first PTS values and the
chosen media-timeline origin explicitly.

Native HLS seeking is implemented by `b0234ab` (optional `PacketSource::seek_to`),
`96e3e49` (packet-source seek/barrier integration and duration propagation), `982f5ae`
(HLS `#EXTINF` segment-time indexing plus per-segment MPEG-TS seeking), and `61a8332`
(Sanctuary playback integration). Sanctuary converts the requested media-relative
position back onto the source transport PTS axis (`timeline_origin + media_position`)
and dispatches it through `ExecutorHandle::seek_with_generation()`. While a seek is
pending it exposes `PlaybackState::Seeking`, pauses OSS/WASAPI/CoreAudio, clears its
video queue, drains/discards pre-seek frames and waits for the matching barriers from
both routed A/V tracks. A successful `SeekFlush` carries the decode-safe MPEG-TS landing
PTS; Sanctuary converts that back to media time, drops/reopens its PCM output so no
pre-seek samples survive, resets video presentation, and lets the first post-seek AAC
PTS establish the new audio-master epoch before normal preroll can resume. A rejected
seek restores the previous position/play state and disables further seeks for that
session. Rapid later seeks supersede older generations, whose stale barriers are ignored.

`242ae29` tightens that behaviour at the Sanctuary boundary for expensive HLS/MPEG-TS
seeks. Sanctuary now permits only one physical source seek to be in flight. Further
keyboard/scrubber requests update the visible target immediately but replace a single
coalesced destination instead of enqueueing more executor generations. When the active
generation's A/V barriers arrive, Sanctuary either finishes there (if the desired target
returned to the same position) or dispatches exactly one new seek to the latest target,
while keeping audio paused and intermediate decoded state discarded. This preserves the
executor's per-generation barrier contract without forcing HLS to perform obsolete HTTP
segment opens/access-point searches. A real paused Twitch regression requested 600,
1200, 1800 and 2400 s back-to-back: only generations 1 and 2 were physically dispatched,
and the final seek landed at 2398.911 s. The OSS stream remained paused throughout.

The HLS source no longer models a VOD as one giant concatenated byte stream. It retains
resolved segment URLs and cumulative `#EXTINF` timing, owns one MPEG-TS demuxer for the
active segment, and on seek jumps directly to the target segment before asking the inner
MPEG-TS demuxer for the nearest video access point at or before the raw target PTS. This
avoids probing the byte lengths of every preceding segment. Playlist `#EXTINF` totals
are also propagated as the player-visible duration.

The audio and video frames arrive through one ordered, bounded session channel, so
back-pressure must be decided for the A/V session as a whole. The first audio version
incorrectly stopped draining that shared channel as soon as the four-frame video target
was full. If audio messages were waiting behind those video frames, the PCM ring could
empty, the audio master clock would stop, and the four future video frames could never
become due: a stable self-stall. The corrected policy keeps draining while **either**
forward target still needs data and stops only when both the video queue (four frames)
and audio queue (about 500 ms) are ready. A separate eight-frame video hard cap drops
excess decoded video if unusual output ordering is needed to reach pending audio; the
audio ring retains its own hard capacity/headroom guard.

Runtime diagnostics are intentionally always available on stderr while native playback
is active. Once per second Sanctuary prints playback state, master-clock position, the
current pump/back-pressure reason, executor/sink completion, video queue depth/front/
back PTS plus received/presented/dropped counts, and audio stream/preroll state, queued
and free PCM duration, played-sample count, and underrun counters. The sink also emits
a rate-limited line when the two-message session channel is full before it blocks. Play,
pause, audio-clock anchoring, preroll completion, and audio device play/pause transitions
are logged as discrete events. These diagnostics are intended to distinguish decoder,
session-channel, video-queue, audio-ring, and device-clock stalls without requiring a
profiler.

Because audio is now authoritative, real A/V sessions currently expose only 1.0x
playback. Pitch-preserving time stretch/tempo control is a separate milestone; the UI
must not move the media clock faster or slower than the samples actually consumed by
the output device.

Before opening playback, Sanctuary now calls `oxideav_hls::inspect_hls()` on the signed
master playlist. That is one bounded GET of the master only: the inspection returns
all non-I-frame variants with already-resolved media-playlist URLs plus bandwidth,
resolution, frame rate, codecs and linked rendition-name/group metadata. Sanctuary
builds its Quality menu from the video variants (resolution-bearing entries), excluding
Twitch's audio-only variant. On the current Twitch shape this exposes `1080p60
(Source)`, `720p60`, `480p`, `360p` and `160p`. The initial rendition remains the HLS
source's existing fixed-selection preference (normally the highest resolution at or
below 720p). Sanctuary then opens that selected **media playlist URL directly**, so the
normal startup path is one master GET followed by one media-playlist GET; the master is
not fetched a second time.

Changing the Quality selection now performs a deliberately simple fixed-rendition
restart. Sanctuary pauses the old audio output, disconnects the old bounded session
receiver (so a sink blocked in `SyncSender::send()` cannot deadlock executor shutdown),
aborts/joins the old executor, drops the old audio device and decoded-frame queue, then
opens a fresh A/V session directly on the already-resolved URL for the selected variant.
The master playlist is therefore not fetched again during a quality change. If the old
session was playing, the replacement session is put back into Playing state after it is
opened; if it was paused it remains paused.

This is intentionally **not** media-time-safe switching yet. Although ordinary HLS
seeking is now wired, the quality-restart path does not yet invoke it: a replacement
rendition still starts from its beginning and Sanctuary resets the playback clock to
zero. Quality switching is also synchronous on the caller for now. The next quality
milestone can reuse the seek path to reopen at the current media position; decoder
overlap/cross-fade and ABR remain later work.

The desktop launcher accepts
`--video <URL-or-ID>` (or a positional video), `--play`/`--autoplay`, and
`--decode-mode cpu|vdpau-readback|vdpau-direct`, using the same `VideoSource::parse`
rules as the in-app Change Video flow. The decode mode defaults to `cpu`.

Muted/paused GhostBSD validation against Twitch VOD `2386400830` confirms the real
integration without producing sound. Both CPU and `vdpau-direct` runs selected the
1280x720/60 HLS rendition, discovered AAC as 44.1 kHz stereo S16, and opened
`sysaudio/oss` at 44.1 kHz stereo. The VDPAU run additionally initialised the NVIDIA
580.173.02 streaming decoder and the four-slot `GLX interop2 -> Vulkan -> wgpu`
bridge. A hardware-free `oxideav-sysaudio` mock regression proves that the 50 ms
preroll gates start, consumed PCM advances the master clock, and an empty ring causes
the clock to remain fixed while the callback emits silence. A separate real-network
seek smoke test was run through `OxidePlayback` while remaining paused throughout:
Twitch 160p requested media 300.000 s, selected segment 29 (`#EXTINF` start 290.290 s),
landed on a video access point at media 298.334 s / raw 368.358 s, then received the
first post-seek AAC epoch at media 298.368 s. The OSS stream never entered Playing.
The temporary network-dependent test was removed afterwards. The full Sanctuary app
suite currently passes 63 tests.

This Twitch web-player GraphQL/Usher protocol is not a stable public playback API,
so all Twitch-specific request shape, client ID and token handling remain isolated
in `twitch.rs` for straightforward future replacement.

## Audio integration

The native audio path is now implemented around the decoder's sink-facing
`CodecParameters`. The real Twitch VOD validation reports authoritative AAC output as
44.1 kHz, two channels, S16; those parameters are passed to `AudioOutput` before PCM
conversion. The sysaudio stream's negotiated format is then checked. Sample-rate
mismatch is handled by OxideAV's polyphase resampler; channel-count mismatch remains a
hard error until Sanctuary has an explicit speaker-layout/remix policy.

The current master clock counts PCM accepted by the device callback, not output
latency. `oxideav-sysaudio::Stream::latency()` is available for future Bluetooth /
network/output-pipeline compensation, but Sanctuary does not yet subtract that value
from video scheduling. Output-device selection, volume controls, surround/downmix
policy, and Android audio output are likewise future application work. In particular,
`oxideav-sysaudio` has no Android backend today, so real A/V opening on Android will
fail cleanly until an Android backend (for example AAudio) is added.

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
is explicitly authorised. Use the sysaudio mock backend for callback/clock tests, or
open real Sanctuary playback paused so the OSS/WASAPI/CoreAudio stream cannot consume
programme PCM.

## Current SanctuaryPlayer follow-ups

1. Apply `VideoSource::start_time` through the real HLS seek path during initial open.
2. Add pitch-preserving audio time-stretch before re-enabling 0.25x-2.0x rates for
   real A/V sessions.
3. Apply output-latency compensation from `oxideav-sysaudio::Stream::latency()` and
   add explicit output-device / channel-layout / downmix policy.
4. Add an Android backend to `oxideav-sysaudio` (or another Sanctuary Android audio
   implementation) before enabling real A/V playback there.
5. Make fixed HLS quality changes media-time-safe by seeking the replacement session to
   the previous media position, then move the reopen off the UI thread; add ABR only
   after that source/session switching boundary is robust.
6. Consider eliminating the final GPU-local image copy in `vdpau-direct` only if wgpu
   can safely own/sample the externally-written image without weakening resource-state
   correctness.
7. Extend HLS support for discontinuities, byte ranges, fMP4/MAP, encryption, live
   reload and other source shapes only as real inputs require them.
