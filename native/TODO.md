# Native TODO

- Re-enable non-1.0x playback rates for real A/V playback with pitch-preserving audio time-stretch and correct A/V clocking.
- Multiple qualities - e.g. after seek show the low quality first then upgrade
- Compensate the media clock for reported audio output latency so presentation timing reflects when sound actually reaches the device.
- Add an independent playback test with deliberately slow video decoding.
- Add an independent playback test with deliberately slow audio decoding.
- Add an independent playback test with deliberately slow video presentation.
- Add an independent playback test with deliberately slow audio presentation.
- Add a manual A/V sync test mode using a deterministic synthetic cue such as a simulated metronome, so presentation offset and drift can be observed and adjusted deliberately.
- Add live-stream playback support
- Add OS media-key handling for play/pause and other appropriate transport controls.
- Add equivalent display sleep/screensaver inhibition on desktop platforms; Android keeps the display awake only during foreground playback.
