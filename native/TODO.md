# Native TODO

## Player UI and interaction

- Update the native window title from the spoiler-sanitised current-video title.
- Represent buffering/rebuffering explicitly in playback state and UI rather than leaving a starved session looking like ordinary `Playing` playback.
- Add user-visible playback/network error reporting and recovery controls instead of relying mainly on stderr diagnostics.
- Improve end-of-media/restart behaviour and expose an intentional replay/restart action rather than only disabling play once `Ended` is reached.
- Keep current quality/rate UI synchronised with asynchronous backend changes such as ABR, source changes or device/player constraints.
- Locked controls should make menu icon appear washed out like the others

## Quality, rate and A/V timing

- Move HLS quality/session reconstruction off the UI thread so a quality change cannot block egui while playlists, segments, decoders and audio output are reopened.
- Add adaptive bitrate (ABR) quality selection based on sustained network/playback conditions, while retaining manual/favourite-quality overrides.
- Re-enable non-1.0x playback rates for real A/V playback with pitch-preserving audio time-stretch and correct A/V clocking.
- Compensate the media clock for reported audio output latency so presentation timing reflects when sound actually reaches the device.

## Playback resilience and lifecycle

- Add playback-level retry/reopen recovery for transient playlist, segment, decoder and network failures without losing the current media position.
- Make suspend/background/resume behaviour explicit: save position, pause/stop audio appropriately, release/recreate graphics/media resources as required, and resume into a consistent prior state.
- Ensure seeking and playback state remain recoverable when a seek target is unavailable, outside the live window, or fails after source/network changes.

## Testing and diagnostics

- Add an independent playback test with deliberately slow video decoding.
- Add an independent playback test with deliberately slow audio decoding.
- Add an independent playback test with deliberately slow video presentation.
- Add an independent playback test with deliberately slow audio presentation.
- Add a manual A/V sync test mode using a deterministic synthetic cue such as a simulated metronome, so presentation offset and drift can be observed and adjusted deliberately.
- Logs to rotating files rather than console, so easier to inspect later

## Saved positions and session persistence

- Force/debounce a saved-position upload after pausing so the final few seconds are not lost when no further playback ticks occur.
- Flush the latest safe saved position when changing video, suspending/backgrounding the app, and shutting down; handle any in-flight save without silently dropping the final position.
- Review saved-position table parity with the web client, including suppressing the current device/current video row when its saved position is effectively the same as the current playback position.
- Decide and implement appropriate session restoration on native restart so reload/relaunch can recover the current video/time where desired.

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
- Implement a real Android audio backend.
- Implement Android audio-focus handling and react correctly to calls, alarms, headset/Bluetooth changes and other audio interruptions.

## Video decode and presentation

- Add hardware video decoding and efficient GPU presentation paths on Windows.
- Add hardware video decoding and efficient GPU presentation paths on Android.
- Finish evaluating whether the final GPU-local copy in the FreeBSD `vdpau-direct` presentation bridge can be removed safely.
- Propagate video colour metadata through decode/presentation and select the correct YUV matrix and full/limited range instead of using one fixed conversion.
- Add transfer-function/colour-primaries handling and HDR/tone-mapping/output support for HDR sources rather than treating all decoded video as SDR.

## Platform integration

- Add OS media-key handling for play/pause and other appropriate transport controls.
- Add platform media-session integration (lock-screen/system media controls and current media metadata) where available.
- Prevent display sleep/screensaver activation while actively playing video, and release the inhibition when paused/stopped/backgrounded.
- Add native deep-link/share support for the current video and media time, equivalent to the web client's continuously updated `videoId`/`time` URL state.

## Documentation

- Update `oxideav-hls/README.md` to describe the current `HlsPacketSource`/`EXTINF` media-time seeking architecture instead of the superseded concatenated-byte-source design.