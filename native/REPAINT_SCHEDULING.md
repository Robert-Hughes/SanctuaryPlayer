# Repaint scheduling

SanctuaryPlayer's desktop event loop is deadline-driven. It uses `ControlFlow::Wait` when no work is pending and `ControlFlow::WaitUntil` for playback, persistence, UI animation, and egui repaint deadlines.

The two loading spinners use SanctuaryPlayer's local paced spinner rather than egui 0.36's stock `Spinner`. The stock widget calls immediate `request_repaint()` while visible, which can bypass the application's frame pacing and render uncapped. The local spinner instead requests its next frame after 16 ms, matching SanctuaryPlayer's existing animation cadence.

Do not reintroduce stock `ui.spinner()` / `egui::Spinner` calls without accounting for this scheduling behaviour.

## GPU surface recovery

Desktop surface acquisition is deliberately recoverable. `wgpu::CurrentSurfaceTexture::Timeout` and `Occluded` skip a frame, while `Outdated` and `Lost` reconfigure the surface. A `Validation` result is also logged and retried through a bounded surface-reconfiguration path instead of immediately terminating SanctuaryPlayer. A successful acquisition resets the validation-failure counter; three consecutive validation failures are allowed before the existing fatal GPU-error path is used.

This is important under heavy system load because long render stalls can coincide with surface-state failures. On 16 September 2026 a desktop session exited after a single `surface acquisition failed validation` result while render gaps had reached multiple seconds. There was no process crash, OOM kill, or NVIDIA reset; SanctuaryPlayer itself called `event_loop.exit()` for the validation status.

The desktop wgpu device therefore installs both an uncaptured-error callback and a device-lost callback. These log the underlying wgpu validation/device error before the higher-level recovery message, so a future recurrence should retain the concrete cause rather than only the generic `Validation` status. Keep these diagnostics even if surface-recovery policy changes.

A reconfiguration requests an immediate redraw so recovery does not depend on a later playback, animation, persistence, or egui deadline.
