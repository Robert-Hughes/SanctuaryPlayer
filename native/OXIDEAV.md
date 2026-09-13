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
- HLS VOD source support with lazy MPEG-TS segment access and one-segment
  successor readahead (`147e0b6`, `65e03ab`, `5e4c400`, `7f0acec`).
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

## Current end-to-end playback architecture

The native player uses **one shared source/demux graph with independent per-track
decode, back-pressure and presentation paths**. Audio and video share one media PTS
timeline, but Sanctuary deliberately does not depend on the order in which the two
tracks happen to produce decoded output.

For the current Twitch/HLS path the graph is:

```text
                    Twitch / selected HLS rendition
                               │
                               ▼
                         shared HLS source
                               │
                               ▼
                         MPEG-TS demuxer
                               │
                  ┌────────────┴────────────┐
                  │                         │
           audio packet queue        video packet queue
                  │                         │
                  ▼                         ▼
             AAC decoder              H.264 decoder
                  │                         │
          terminal owns sink         terminal owns sink
                  │                         │
                  ▼                         ▼
           AudioTrackSink             VideoTrackSink
                  │                         │
       bounded Sanctuary channel   bounded Sanctuary channel
                  │                         │
                  ▼                         ▼
      timestamped PCM timeline     two-frame FrameLease queue
                  │                         │
                  ▼                         ▼
       sysaudio callback clock      VideoClock + deadlines
                  │                         │
                  ▼                         ▼
        OSS / WASAPI / etc.        winit + wgpu presentation
```

### Demuxing and bounded upstream coupling

The selected HLS media playlist is opened once and feeds one active MPEG-TS
demuxer. TS packets are read sequentially, while audio and video PIDs have
independent PES reassembly state. A completed audio PES and a completed video PES
therefore do not form a useful global delivery sequence: **PTS is authoritative media
time**, not cross-track callback order.

`5e4c400` keeps this shared-source model while removing synchronous HTTP setup from
normal segment boundaries. HLS maintains exactly one prepared successor outside the
decoded A/V queues: a background worker opens the next segment and primes its MPEG-TS
demuxer through the first packet. At EOF the prepared demuxer is installed directly.
This readahead is intentionally independent of downstream backpressure and remains
bounded to one successor rather than increasing decoded-frame or PCM lookahead.

After demuxing, each routed track has its own bounded compressed-packet queue before
its decoder. This lets one decoder or presenter lag temporarily without immediately
stopping its sibling. The independence is intentionally bounded, however: if one
track remains back-pressured long enough, its packet queue eventually fills and the
shared demuxer blocks before it can reach later packets for the sibling track.

OxideAV's general pipeline default is 16 compressed packets per track. Sanctuary
intentionally overrides only that packet depth to **256 per track** while leaving the
frame-channel default unchanged. Real Twitch MPEG-TS VODs have been observed with
about 3.8 seconds of valid physical mux skew between audio and video; 16 packets is
only roughly 0.34 seconds of AAC or 0.53 seconds of 30 fps video and therefore causes
head-of-line starvation in a shared demux graph. The larger Sanctuary-specific bound
is compressed demux slack, not presentation lookahead: decoded video remains capped
at two frames and the PCM target is unchanged. Keeping this override in Sanctuary
rather than raising OxideAV's global default preserves the tighter general-purpose
memory bound for pipelines that do not need Twitch-scale mux-skew tolerance.

### JobSink and independent TrackSinks

`JobSink` remains the lifecycle owner for one logical output. For native playback it
opts into one independently blocking `TrackSink` per primary pipeline track:

```text
JobSink
├── AudioTrackSink
└── VideoTrackSink
```

The **terminal processing stage owns its TrackSink**. On Sanctuary's direct playback
routes that means:

```text
AAC decoder  -> AudioTrackSink
H.264 decoder -> VideoTrackSink
```

There is no OxideAV decoded-output queue or central mux worker merely to hand an
already-final frame to the application. Genuine processing boundaries still retain
their queues: demux/source -> decoder packets, decoder -> real filter frames,
filter -> encoder frames, and frame-source fan-out.

A blocked video TrackSink therefore blocks the video terminal worker without
immediately blocking audio, and vice versa. Blocking TrackSinks receive the executor's
shared `CancellationToken` and must make their waits cancellation-aware so stop or a
sibling failure cannot deadlock teardown.

`StreamUpdate` and seek barriers travel through the same TrackSink path as that
track's media. Ordering is strict **within a track** but intentionally undefined
between tracks.

### Audio presentation and clock

AAC may initially have incomplete PCM metadata. The decoder first emits an ordered
`TrackSink::stream_update()` once sample rate, channel count and sample format are
authoritative; Sanctuary opens `AudioOutput` only after that update.

Decoded audio is converted to interleaved f32 and written into the timestamp-aware
ring in `audio_timeline.rs`. Frame PTS is rescaled onto the output device's integer
sample-frame clock. Contiguous PCM appends normally; forward gaps are represented by
zero PCM; stale data is discarded; partial overlaps are trimmed; missing PTS is
treated as contiguous with the current expected end.

If a complete next audio frame cannot fit, Sanctuary retains that exact frame as
`pending_audio_frame` and stops draining **the audio TrackSink channel only** until
callback consumption frees space. Video remains independent.

The sysaudio callback owns `next_output_pts`. Every callback advances that cursor by
the full requested block duration, including periods filled with silence, so audio
presentation time continues even through a decode miss. The device is held paused
until roughly 50 ms of preroll is ready. On normal playback Sanctuary currently aims
for roughly 500 ms of queued PCM.

### Video presentation and clock

Decoded video remains in `FrameLease` ownership. Sanctuary keeps a strict two-frame
presentation lookahead. Once those two slots are full it stops draining the video
TrackSink channel; the next decoded frame stays upstream and naturally back-pressures
the video decoder. There is no extra `pending_video_frame` slot and no larger hidden
decoded-frame buffer used merely to keep audio moving.

`VideoClock` anchors an integer media PTS to a high-resolution `Instant`. Desired
video PTS at a later wall-clock instant is derived from that anchor. If the next frame
is in the future, Sanctuary asks winit to wake at its exact presentation deadline
rather than polling continuously. If more than one queued frame is already due, older
due frames may be dropped so the newest currently-due frame is presented. Future
frames are not discarded merely to relieve back-pressure. A video frame without PTS
is dropped because it cannot be scheduled meaningfully.

The public playback position is currently derived from this video clock (except while
a seek is explicitly in flight). Pause freezes the video PTS/Instant mapping and
pauses sysaudio; resume establishes a new wall-clock anchor without counting paused
time as media time.

### Current A/V synchronisation model

Audio and video are both mapped onto the same media-relative timeline derived from
their MPEG-TS PTS values and the chosen timeline origin:

```text
                 common media PTS timeline
                    /               \
                   /                 \
        audio sample clock        video PTS clock
          (sysaudio callback)      (Instant deadlines)
```

There is **not yet an active A/V drift controller** that compares the two clocks and
nudges one presentation rate toward the other. At 1.0x each side currently presents at
its nominal rate against the common PTS timeline. Adding measured drift correction,
output-latency compensation and later variable-rate playback is a separate layer above
this transport/decode/back-pressure architecture.

Seeking does not depend on cross-track arrival order. A seek generation is carried
down each routed track as an ordered barrier. Sanctuary discards old-epoch state and
does not complete the seek until the matching barriers required from the routed A/V
tracks have arrived. The landed transport PTS is then converted back onto the common
media-relative timeline before presentation resumes.
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
starts an `Executor` with a Sanctuary-owned `JobSink`. `@display` requests both the
H.264 video track and the AAC audio track, so source acquisition and MPEG-TS demux remain
one shared graph.

For staged playback the `JobSink` opts into OxideAV's independent-track output mode.
It creates one `TrackSink` for audio and one for video, each backed by its own bounded
Sanctuary channel. The terminal worker of each OxideAV track calls its `TrackSink`
directly: a direct decoder therefore has no decoded-frame output queue or mux worker
between `receive_frame_lease()` and Sanctuary. Real decoder→filter and filter→encoder
queues remain because they are genuine processing-stage boundaries. Blocking the video
TrackSink backpressures only the video track; audio continues independently until bounded
pressure eventually propagates through the video packet queue to the shared demuxer.

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
decoded frame as the audio head of line and stops draining its audio TrackSink channel
until callback consumption creates enough room to retry it. This audio-local ordering
does not block video delivery.

Initial sink-facing audio metadata can be provisional for in-band configured codecs.
Sanctuary therefore does not open `AudioOutput` from `JobSink::start()`. The audio
track emits an ordered `TrackSink::stream_update()` after the decoder has consumed a
packet and learned its actual PCM shape; Sanctuary opens the device only once rate,
channels and sample format are authoritative, before the corresponding decoded frame can
arrive. `AudioOutput` then keeps the device paused initially and uses a 50 ms PCM
preroll before it may start. A negotiated sample-rate or channel-count change is
currently a hard error: resampling/remixing is deliberately deferred until its timestamp
semantics are designed explicitly. The ring is sized for roughly four seconds.

The sysaudio callback owns a `next_output_pts` cursor. For each requested destination
block it first discards queued PCM older than that cursor, emits silence when the ring is
empty or begins in the future, copies aligned PCM where available, and finally advances
`next_output_pts` by the full requested block duration. Therefore a decoder miss does
not stop audio time: silence occupies the missed presentation interval and late decoded
samples are subsequently discarded or trimmed.

Audio timing and video timing are now deliberately separate. The audio callback advances
`next_output_pts` on the integer sample clock, while `VideoClock` maps video PTS to
high-resolution wall-clock deadlines. The public playback position is derived from the
video clock outside an in-flight seek. Both clocks are mapped to the same media-relative
PTS origin, but Sanctuary does not yet run an active drift-correction controller between
them. Output-device latency is likewise not yet applied to the audio presentation clock.

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
PTS; Sanctuary converts that back to media time and discards the old PCM output so no
pre-seek samples survive. If the current audio metadata is already authoritative it
reopens the output immediately; a freshly opened quality rendition may still have
provisional AAC metadata, in which case Sanctuary leaves audio closed until the first
ordered post-seek `StreamUpdate` supplies rate/channels/format. The first post-seek AAC
PTS then initialises the new audio timeline before normal preroll can resume. A rejected seek
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
resolved segment URLs and cumulative `#EXTINF` timing, owns one active MPEG-TS demuxer
plus one HLS-local prepared successor, and on seek jumps directly to the target segment
before asking the inner MPEG-TS demuxer for the nearest video access point at or before
the raw target PTS. A successful landing invalidates the previous successor slot and
starts preparation after the actual landed segment; readahead failures are retained until
the successor is needed. This avoids both probing every preceding segment length and the
normal synchronous next-segment HTTP startup at EOF. Playlist `#EXTINF` totals are also
propagated as the player-visible duration.

Audio and video now arrive through separate bounded TrackSink channels. Sanctuary makes
no assumption about cross-track callback interleaving: PTS is the media timeline, while
ordering is guaranteed only within each track. Normal video presentation retains a
two-frame decoded lookahead. When those two slots are full Sanctuary stops draining the
video TrackSink channel; the next decoded frame therefore remains upstream and
backpressures the video decoder instead of being copied into an extra hidden video
buffer or discarded merely to service audio. Audio can continue filling its PCM
timeline independently.

Timestamp-aware audio has its own head-of-line rule. If the next decoded audio frame
cannot yet fit, it is retained as `pending_audio_frame` and Sanctuary stops draining
later messages from the audio TrackSink channel until that exact frame is accepted.
Video remains independent. Seek barriers and decoder stream updates travel through the
same TrackSink as their media payloads, preserving the required per-track order; seek
completion still waits for the matching barrier from every routed A/V track.

Runtime diagnostics are intentionally always available on stderr while native playback
is active. Once per second Sanctuary prints playback state, current player position, the
pump/back-pressure reason, executor/sink completion, video queue depth/front/back PTS
plus received/presented/dropped counts, and audio stream/preroll state, queued/free PCM
duration, submitted sample count, `next_output_pts`, and underrun counters. Each
TrackSink also emits a rate-limited line when its bounded Sanctuary channel is full
before it blocks. Play, pause, timeline anchoring, preroll completion, and audio device
play/pause transitions are logged as discrete events.

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

Muted GhostBSD validation against Twitch VOD `2386400830` confirms the current
independent-track architecture without producing sound. The initial MPEG-TS AAC stream
arrives with unknown rate/channels; the first ordered audio `stream_update()` reports
the actual **48 kHz stereo S16** output, and only then does Sanctuary open
`sysaudio/oss` at 48 kHz stereo.

Real `vdpau-direct` regressions after the TrackSink split sustained approximately
**30 fps at 160p** and **59.9-60.0 fps at 720p60**, kept the normal two-frame video
lookahead and roughly 500 ms audio target, and recorded **zero audio underrun callbacks
and zero underrun samples** after startup. A real initial seek request to 60 s delivered
the matching per-track barriers, landed at the decode-safe point around 58.094 s, and
resumed stable 160p playback with zero underruns.

The timestamp-aware audio path is covered by deterministic mock/ring tests for contiguous
PTS, missing PTS, overlap, late frames, gaps, zero padding, whole-frame deferral,
underflow, stale-buffer discard, future-ring silence, stereo sample-frame accounting and
audio-local head-of-line back-pressure. Playback regressions additionally pin independent
audio/video TrackSink progress, cancellation of a blocked TrackSink, two-frame video
back-pressure, authoritative late audio format discovery and multi-track seek barriers.
The Sanctuary app suite currently passes **117 tests**.

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

### HLS successor-readahead validation

After `5e4c400`, a muted real Twitch VOD regression using the native VDPAU path
crossed the roughly 10 s and 20 s segment boundaries without source starvation:
`underrun_callbacks` and `underrun_samples` remained zero, the audio queue stayed
around its normal 500 ms target, and the video dropped-frame count remained at the
three startup drops rather than jumping by a segment-sized batch. This directly
regresses the previous boundary failure where both A/V queues emptied and playback
then discarded overdue media to catch up.

### Twitch mux-skew packet-slack validation

VOD `2859508682` exposed a separate shared-demux head-of-line problem even with HLS
successor readahead working. Its MPEG-TS segments carry several seconds of valid
physical A/V interleave skew (about 3.5 s in the first 160p segment and about 3.8 s
in inspected 480p segments), far beyond OxideAV's default 16-packet per-track slack.
Before the Sanctuary override, the first 160p playback had fallen from about 515 ms
audio queued to about 219 ms by 1.07 s and had accumulated 16 underrun callbacks by
roughly 1.9 s, well before any HLS segment boundary.

With Sanctuary requesting 256 compressed packets per track, the same muted
`vdpau-direct` VOD held around its normal 500 ms audio target through steady playback
and across the first HLS boundary, with no alternating audio/video queue collapse. In
a longer run five startup underrun callbacks occurred before steady state, but the
counter then remained unchanged through 13 s. This validates the larger packet depth
as mux-interleave slack rather than presentation buffering.

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

1. Add an explicit A/V drift controller above the existing independent clocks. Measure
   audio presentation time (including device latency) against `VideoClock`, then apply a
   bounded correction policy rather than relying indefinitely on nominal-rate clocks.
2. Add pitch-preserving audio time-stretch before re-enabling 0.25x-2.0x rates for
   real A/V sessions, using the same controller model for rate changes.
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
