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
interleaved, and merged into a timestamp-aware bounded SPSC PCM ring implemented in
`native/app/src/audio_timeline.rs`. The platform callback is supplied by
`oxideav-sysaudio`: FreeBSD/GhostBSD uses native OSS (`/dev/dsp`), Windows uses
WASAPI, macOS uses CoreAudio, and Linux uses the first working configured backend.
Sanctuary does not contain an OSS-specific device implementation.

The audio ring's timeline is expressed as integer device-rate sample-frame PTS values.
Each decoded frame PTS is rescaled from the stream time base onto that integer sample
clock before queueing. Contiguous frames append directly; forward gaps are padded with
zero PCM; fully stale frames are dropped; partially overlapping frames have only their
already-covered prefix discarded. A frame without a PTS is treated as contiguous with
the current expected end. If a gap or frame cannot yet fit, Sanctuary retains that exact
decoded frame as the audio head of line, fills as much of the gap with zeroes as capacity
permits, and stops draining later session messages until callback consumption creates
enough room to retry it.

Initial sink-facing audio metadata can be provisional for in-band configured codecs.
Sanctuary therefore does not open `AudioOutput` from `JobSink::start()`. The staged
pipeline emits an ordered `stream_update()` after the decoder has consumed a packet and
learned its actual PCM shape; Sanctuary opens the device only once rate, channels and
sample format are all authoritative, before the corresponding decoded frame can arrive.
`AudioOutput` then keeps the device paused initially and uses a 50 ms PCM preroll before
it may start. A negotiated sample-rate or channel-count change is currently a hard error:
resampling/remixing is deliberately deferred until its timestamp semantics are designed
explicitly. The ring is sized for roughly four seconds. Normal A/V pumping still stops
when both forward targets are ready, while a near-full audio ring and a deferred
head-of-line frame provide additional upstream back-pressure.

The sysaudio callback owns a `next_output_pts` cursor. For each requested destination
block it first discards queued PCM older than that cursor, emits silence when the ring is
empty or begins in the future, copies aligned PCM where available, and finally advances
`next_output_pts` by the full requested block duration. Therefore a decoder miss does
not stop audio time: silence occupies the missed presentation interval and late decoded
samples are subsequently discarded or trimmed.

The wider wall-clock/controller A/V clock redesign is intentionally not part of this
change. For compatibility, the current player position is still derived from the audio
timeline while an audio track is present, now using `next_output_pts` rather than a
count of real PCM popped from the ring. Output latency is also not yet applied. Pause
pauses the sysaudio stream, so this temporary position still freezes naturally. Media
without audio retains the existing video-only fallback.

`2fe9a28` in `oxideav-pipeline` is required for that fallback to be trustworthy. The
pipeline previously synthesised sink-facing primary streams with `start_time: Some(0)`
regardless of source metadata. Twitch MPEG-TS starts on a non-zero transport clock
(the observed VOD first segment is around 70.024 s audio / 70.060 s video), so the
fabricated zero made Sanctuary queue video around 70 s in the future while its then
audio-derived clock started at zero. The pipeline now preserves a known source start
(rescaled when necessary) and leaves an unknown start as `None`; Sanctuary then anchors
from the first actual decoded A/V PTS values. Runtime logs print those first PTS values and the
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
PTS initialise the new audio timeline before normal preroll can resume. A rejected seek
restores the previous position/play state and disables further seeks for that session.
Rapid later seeks supersede older generations, whose stale barriers are ignored.

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
back-pressure must be decided for the A/V session as a whole. Normal playback keeps
draining while **either** forward target still needs data and stops when both the video
queue (four frames) and audio queue (about 500 ms) are ready. A separate eight-frame
video hard cap still prevents unusual output ordering from growing video without bound.

Timestamp-aware audio adds one stronger ordering rule. If the next decoded audio frame
cannot yet fit, it is retained as `pending_audio_frame` and no later shared-channel
message is consumed until that same frame is accepted. This is the application-level
equivalent of leaving the decoded frame at the head of the queue, and lets a large PTS
gap be materialised incrementally as zero PCM without allowing later audio/video to
overtake it.

Runtime diagnostics are intentionally always available on stderr while native playback
is active. Once per second Sanctuary prints playback state, current player position, the
pump/back-pressure reason, executor/sink completion, video queue depth/front/back PTS
plus received/presented/dropped counts, and audio stream/preroll state, queued/free PCM
duration, submitted sample count, `next_output_pts`, and underrun counters. The sink
also emits a rate-limited line when the two-message session channel is full before it
blocks. Play, pause, timeline anchoring, preroll completion, and audio device play/pause
transitions are logged as discrete events.

Real A/V sessions currently expose only 1.0x playback. Pitch-preserving time
stretch/tempo control remains a separate milestone; the forthcoming wall-clock A/V
controller will define how independent audio/video presentation rates are adjusted.

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

Changing the Quality selection performs a fixed-rendition restart while preserving
media time. Sanctuary captures the current media position and prior play/pause intent,
pauses and tears down the old session, then opens a fresh A/V session directly on the
already-resolved URL for the selected variant. The master playlist is therefore not
fetched again during a quality change. Before the replacement session is allowed to
play, Sanctuary dispatches an HLS media-time seek to the captured position and waits for
the replacement session's seek barriers. A previously playing session resumes only
after that seek lands; a paused session remains paused. As with ordinary seeking, the
reported position may move slightly backwards to the decode-safe access point selected
by MPEG-TS/H.264.

A paused live Twitch regression against VOD `2386400830` switched from 160p at
298.334 s to 360p. The replacement rendition landed at the same 298.334 s decode-safe
point; after its first post-seek audio timestamp established the new audio epoch, the
reported clock was 298.859 s. The OSS stream remained paused throughout the test.
Quality switching is still synchronous on the caller while the replacement playlist,
segment, decoder and audio output are opened; moving that reconstruction off the UI
thread remains separate work. Decoder overlap/cross-fade and ABR also remain later work.

The desktop launcher accepts
`--video <URL-or-ID>` (or a positional video), `--play`/`--autoplay`,
`--mute`, and `--decode-mode cpu|vdpau-readback|vdpau-direct`, using the same
`VideoSource::parse` rules as the in-app Change Video flow. The decode mode defaults
to `cpu`. `--mute` sets sysaudio's per-stream software gain to zero while leaving
the audio callback and timestamp timeline active.

Muted GhostBSD validation against Twitch VOD `2386400830` confirms the corrected
in-band format discovery without producing sound. The initial MPEG-TS AAC stream now
arrives with unknown rate/channels rather than a guessed 44.1 kHz shape; the first
decoder `stream_update()` reports the actual **48 kHz stereo S16** output, and only then
does Sanctuary open `sysaudio/oss` at 48 kHz stereo. A muted `vdpau-direct` playing
smoke kept the PCM ring near its 500 ms forward target and advanced `next_output_pts` at
48 kHz. Nine startup underrun callbacks occurred while first VDPAU presentation stalled
for about 539 ms; the count then remained stable, so that is a separate preroll/startup
issue rather than continuous PCM loss.

The timestamp-aware audio path is covered by deterministic mock/ring tests: contiguous
PTS, missing PTS, partial/full overlap, late frames after underrun, small and ring-spanning
gaps, incremental zero padding, whole-frame deferral on capacity pressure, empty/partial
underflow, stale-buffer discard, future-ring silence, stereo sample-frame accounting and
head-of-line back-pressure. A player regression also pins deferred device opening from an
authoritative 48 kHz stream update. The Sanctuary app suite currently passes 103 tests.

This Twitch web-player GraphQL/Usher protocol is not a stable public playback API,
so all Twitch-specific request shape, client ID and token handling remain isolated
in `twitch.rs` for straightforward future replacement.

## Audio integration

The native audio path is implemented around authoritative decoder `CodecParameters`
plus the stream `TimeBase`. The initial sink description may contain unknown fields;
`stream_update()` supplies the post-decode PCM shape before Sanctuary accepts the first
audio frame. Incoming audio PTS values are then rescaled onto integer output-sample-clock
ticks and the PCM ring carries that timeline explicitly. The sysaudio stream's negotiated
format is checked before playback; sample-rate and channel-count mismatches are both hard
errors for now so no resampler can obscure the timestamp model while this work is being
established.

`next_output_pts` describes the PTS immediately after the block most recently supplied
to sysaudio, including any silence supplied for missing/late decoded audio. It is a
submission-side timeline, not an estimate of what the listener hears. The already
available `oxideav-sysaudio::Stream::latency()` API will be incorporated later when the
wall-clock A/V controller is designed. Output-device selection, volume controls,
surround/downmix policy, and Android audio output are likewise future application work.
In particular, `oxideav-sysaudio` has no Android backend today, so real A/V opening on
Android will fail cleanly until an Android backend (for example AAudio) is added.

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
is explicitly authorised. Use the sysaudio mock backend for callback/timeline unit tests,
or pass `--mute` for real playing-state Sanctuary regressions so the audio callback and
timestamp timeline still advance without audible programme PCM. Paused playback remains
suitable when callback advancement is not required.

## Current SanctuaryPlayer follow-ups

1. Apply `VideoSource::start_time` through the real HLS seek path during initial open.
2. Add pitch-preserving audio time-stretch before re-enabling 0.25x-2.0x rates for
   real A/V sessions.
3. Apply output-latency compensation from `oxideav-sysaudio::Stream::latency()` and
   add explicit output-device / channel-layout / downmix policy.
4. Add an Android backend to `oxideav-sysaudio` (or another Sanctuary Android audio
   implementation) before enabling real A/V playback there.
5. Move fixed HLS quality reopen off the UI thread; add ABR only after that
   source/session switching boundary is robust.
6. Consider eliminating the final GPU-local image copy in `vdpau-direct` only if wgpu
   can safely own/sample the externally-written image without weakening resource-state
   correctness.
7. Extend HLS support for discontinuities, byte ranges, fMP4/MAP, encryption, live
   reload and other source shapes only as real inputs require them.
