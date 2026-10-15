# Native TODO

- Show video ID somewhere (no longer available in the URL!)
- Some video titles are not showing in the Saved Positions list
- Re-enable non-1.0x playback rates for real A/V playback with pitch-preserving audio time-stretch and correct A/V clocking.
- Multiple qualities - e.g. after seek show the low quality first then upgrade
- Maintain a buffer of old frames for quick seeking backwards e.g. 10 secs?
- Compensate the media clock for reported audio output latency so presentation timing reflects when sound actually reaches the device.
- Add an independent playback test with deliberately slow video decoding.
- Add an independent playback test with deliberately slow audio decoding.
- Add an independent playback test with deliberately slow video presentation.
- Add an independent playback test with deliberately slow audio presentation.
- Add a manual A/V sync test mode using a deterministic synthetic cue such as a simulated metronome, so presentation offset and drift can be observed and adjusted deliberately.
- Improve sliding-window live-HLS seeking: keep 0:00 anchored to the initial playlist timeline rather than moving it when old segments expire; track the currently available earliest and latest positions as the playlist slides; clamp or reject seeks outside that seekable range instead of attempting to fetch expired segments. Distinguish the moving live edge from the fixed playback timeline. (Growing live/event HLS playlist reload and playback support is implemented.)
- Add OS media-key handling for play/pause and other appropriate transport controls.
- Add equivalent display sleep/screensaver inhibition on desktop platforms; Android keeps the display awake only during foreground playback.
