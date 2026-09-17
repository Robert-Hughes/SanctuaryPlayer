# Twitch Visibility & Autoplay Enforcement Findings

This document outlines the technical logic used by Twitch to detect visibility violations and enforce autoplay/viewing policies in their embedded players, based on analysis of the `98722-4d5b7dc8b130d8bf7d1a.js` asset.

## 1. Core Logic: `detectVisibilityViolation`
The function `detectVisibilityViolation` is part of Twitch's security layer. It compares the current player state against a set of "Embed Visibility" rules.

### Trigger Mechanism
*   **Enforcement State:** Only active while `isPlayerPlaying` is true.
*   **Lifecycle:** Triggered inside React's `componentDidUpdate` whenever the `embedVisibilityContext` changes.
*   **Action:** If a violation is confirmed, the player executes `props.pause()` and reports an `embed_play_block` analytics event.

---

## 2. Detection Mechanism: `IntersectionObserver`
Twitch uses a native browser `IntersectionObserver` to feed data into the visibility context.

### Configuration
*   **Thresholds:** `[0, 0.5, 1]` — Triggers report updates when 0%, 50%, or 100% of the player is visible.
*   **Delay:** `1000ms` — **Critical.** The observer waits 1 second for the visibility to remain stable before reporting. This explains why the player often pauses ~1 second after starting.
*   **Track Visibility:** `trackVisibility: true` — Enables hardware-level checks for opacity and occlusion (supported in Chromium-based browsers).

---

## 3. Violation Types & Reasons
When `detectVisibilityViolation` runs, it evaluates the following:

| Reason | Condition |
| :--- | :--- |
| `style-visibility` | `isVisible` is `false`. Triggered by CSS `opacity: 0`, `visibility: hidden`, or `content-visibility`. |
| `size` | Player dimensions (`bCRWidth` / `bCRHeight`) are below the dynamic threshold (often < 400x300px). |
| `viewport-visibility` | `intersectionRatio` is below the threshold (often < 0.5 or 1.0 depending on policy). |
| `block-list` | The embedding domain is flagged for restricted autoplay. |

---

## 4. Key Diagnostic: The `isVisible: false` Trap
Debugging revealed a state where:
*   `intersectionRatio: 1` (Player is fully in the viewport)
*   `isVisible: false` (Violation triggered)

### Root Causes for `isVisible: false`
In modern browsers, `isVisible` can be false even if the player is on screen if:
1.  Any **parent container** has `opacity: 0` or `visibility: hidden`.
2.  The element is subject to **browser throttling** (e.g., the tab is in the background or the window is minimized).
3.  The browser's rendering engine has optimized the element away (e.g., `content-visibility: auto` on a parent).

---

## 5. Limitations & Blind Spots
*   **Z-Index Overlays:** Twitch's current observer-based logic **cannot detect** if an external element (like a separate `div`) is sitting on top of the player. As long as the player's own styles are "visible" and it is in the viewport, it passes the check.
*   **Rounding:** The `intersectionRatio` is rounded to the nearest 10th (`Math.round(10 * r) / 10`). This can lead to false positives if the player is slightly clipped by a parent container.

## 6. Recommended Fixes for SanctuaryPlayer
*   **Initialization Delay:** Ensure the player is fully visible and animations are complete before calling `play()`, or account for the 1000ms "stable" window.
*   **Style Audit:** Ensure no parent of the player uses `opacity: 0` or `visibility: hidden` for layout management. Use `rgba(0,0,0,0)` or off-screen positioning instead if items must be "hidden" but functional.
*   **Background Management:** Be aware that switching tabs or minimizing the SanctuaryPlayer window may flip `isVisible` to `false`, causing an intended (or unintended) pause.
