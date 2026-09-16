# Repaint scheduling

SanctuaryPlayer's desktop event loop is deadline-driven. It uses `ControlFlow::Wait` when no work is pending and `ControlFlow::WaitUntil` for playback, persistence, UI animation, and egui repaint deadlines.

The two loading spinners use SanctuaryPlayer's local paced spinner rather than egui 0.36's stock `Spinner`. The stock widget calls immediate `request_repaint()` while visible, which can bypass the application's frame pacing and render uncapped. The local spinner instead requests its next frame after 16 ms, matching SanctuaryPlayer's existing animation cadence.

Do not reintroduce stock `ui.spinner()` / `egui::Spinner` calls without accounting for this scheduling behaviour.
