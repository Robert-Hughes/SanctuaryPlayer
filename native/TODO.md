# Native TODO

## Player UI and interaction

- Update the native window title from the spoiler-sanitised current-video title.
- Represent buffering/rebuffering explicitly in playback state and UI rather than leaving a starved session looking like ordinary `Playing` playback.
- Add user-visible playback/network error reporting and recovery controls instead of relying mainly on stderr diagnostics.
- Improve end-of-media/restart behaviour and expose an intentional replay/restart action rather than only disabling play once `Ended` is reached.
- Keep current quality/rate UI synchronised with asynchronous backend changes such as source changes or device/player constraints.
- Locked controls should make menu icon appear washed out like the others

## Quality, rate and A/V timing

- Re-enable non-1.0x playback rates for real A/V playback with pitch-preserving audio time-stretch and correct A/V clocking.
- Compensate the media clock for reported audio output latency so presentation timing reflects when sound actually reaches the device.

## Playback resilience and lifecycle

- Add playback-level retry/reopen recovery for transient playlist, segment, decoder and network failures without losing the current media position.
- Continue hardening suspend/background/resume behaviour beyond the current Android pause-on-background policy: release/recreate graphics/media resources as required and resume into a consistent prior state.
- Ensure seeking and playback state remain recoverable when a seek target is unavailable, outside the live window, or fails after source/network changes.

## Testing and diagnostics

- Add an independent playback test with deliberately slow video decoding.
- Add an independent playback test with deliberately slow audio decoding.
- Add an independent playback test with deliberately slow video presentation.
- Add an independent playback test with deliberately slow audio presentation.
- Add a manual A/V sync test mode using a deterministic synthetic cue such as a simulated metronome, so presentation offset and drift can be observed and adjusted deliberately.

## Sources and streaming

- Implement native YouTube playback rather than only recognising YouTube IDs/URLs.
- Support YouTube VOD and live-source extraction/resolution without depending on the official embedded player.
- Define native YouTube quality selection now that the iframe player's own quality UI/local state is no longer available.
- Add live-stream playback support, including HLS live-playlist reload, moving live windows, safe start-position semantics and live seeking where available.
- Extend HLS support for discontinuities.
- Extend HLS support for byte-range segments.
- Extend HLS support for `#EXT-X-MAP` / fragmented MP4 segments.
- Extend HLS support for encrypted segments and key retrieval where required by supported sources.
- Extend HLS handling for additional master/media playlist shapes, including nested/alternate rendition structures as real sources require them.
- Add Twitch authentication/session support if private, subscriber-only, age-gated or otherwise restricted media needs to work natively.
- Add equivalent authentication/source-session support for YouTube content that cannot be resolved anonymously, if required.

## Tracks, subtitles and audio

- Add subtitle/caption discovery, selection, decoding/timing and rendering.
- Add selection of multiple audio tracks where media exposes them.
- Add application volume and mute controls rather than relying solely on the operating-system/device volume.
- Add audio channel-layout handling and explicit downmix/upmix policy instead of failing when the output device negotiates a different channel count.
- Add audio output-device selection and robust handling of default-device changes, disconnects and reconnects.

## Video decode and presentation

- Add hardware video decoding and efficient GPU presentation paths on Windows.
- Complete real-device validation and hardening of Android MediaCodec readback/direct AHardwareBuffer presentation, including 720p60, seek/reset, colour, orientation and Vulkan validation.
- Finish evaluating whether the final GPU-local copy in the FreeBSD `vdpau-direct` presentation bridge can be removed safely.
- Add transfer-function/colour-primaries handling and HDR/tone-mapping/output support for HDR sources rather than treating all decoded video as SDR.

## Platform integration

- Add OS media-key handling for play/pause and other appropriate transport controls.
- Add platform media-session integration (lock-screen/system media controls and current media metadata) where available.
- If intentional Android background playback is added, implement it as a supported media mode with a foreground media service and media notification rather than relying on Activity survival.
- Add Android picture-in-picture support so video can remain visibly active when the user deliberately leaves the full Activity.
- Prevent display sleep/screensaver activation while actively playing video, and release the inhibition when paused/stopped/backgrounded.
- Add native deep-link/share support for the current video and media time, equivalent to the web client's continuously updated `videoId`/`time` URL state.
