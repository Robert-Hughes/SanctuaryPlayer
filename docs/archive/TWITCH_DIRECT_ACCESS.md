# Twitch Direct VOD Access Analysis

This document outlines the findings for programmatically accessing Twitch VOD streams directly via their internal APIs, bypasses, and security considerations.

## 1. The API Flow

To play a Twitch VOD without the official embed player, you must follow this three-step authentication and discovery process:

### Step 1: Obtain Playback Access Token
Twitch requires a signed token to authorize access to HLS streams. This is requested via their internal GraphQL API.

*   **Endpoint:** `https://gql.twitch.tv/gql`
*   **Header:** `Client-ID: kimne78kx3ncx6brgo4mv6wki5h1ko` (Standard Twitch Web Client ID)
*   **Method:** `POST`
*   **Payload:**
    ```json
    {
      "operationName": "PlaybackAccessToken_Template",
      "query": "query PlaybackAccessToken_Template($login: String!, $isLive: Boolean!, $vodID: ID!, $isVod: Boolean!, $playerType: String!) { videoPlaybackAccessToken(id: $vodID, params: {platform: \"web\", playerBackend: \"mediaplayer\", playerType: $playerType}) @include(if: $isVod) { value signature } }",
      "variables": {
        "isLive": false,
        "login": "",
        "isVod": true,
        "vodID": "VOD_ID",
        "playerType": "embed"
      }
    }
    ```

### Step 2: Retrieve Master Playlist (M3U8)
Use the `value` (token) and `signature` from Step 1 to request the master HLS manifest.

*   **Endpoint:** `https://usher.ttvnw.net/vod/{VOD_ID}.m3u8`
*   **Query Parameters:**
    *   `nmask`: `1`
    *   `sig`: `{signature_from_step_1}`
    *   `token`: `{value_from_step_1_url_encoded}`
    *   `allow_source`: `true`

### Step 3: Select Resolution and Fetch Segments
The Master Playlist returns a list of "Variant Playlists" (1080p, 720p, etc.). Each variant contains a list of `.ts` video segments (usually 10 seconds long).

---

## 2. Technical Findings (Example VOD 2768543459)

*   **Master Playlist URL:** `https://usher.ttvnw.net/vod/2768543459.m3u8?nmask=1&sig=...&token=...`
*   **720p60 Variant Playlist:** `https://dgeft87wbj63p.cloudfront.net/.../720p60/index-muted-2ZESWCP0AI.m3u8`
*   **Example Video Segment:** `https://dgeft87wbj63p.cloudfront.net/.../720p60/0-muted.ts`

---

## 3. Browser Security & CORS Analysis

### The Problem
If you attempt to make these requests directly from `sanctuaryplayer.robdh.uk` (or `localhost`) using `fetch()` or `XMLHttpRequest`, the browser will block them due to **CORS (Cross-Origin Resource Sharing)**.

*   **`gql.twitch.tv`**: Blocks requests from non-Twitch domains.
*   **`usher.ttvnw.net`**: Blocks requests from non-Twitch domains.
*   **`*.cloudfront.net` (Video Segments)**: Generally **OK**. The CDN is usually configured to allow cross-origin requests so that web players can stream the video data.

### Why Twitch's Site Works
Twitch's official frontend sends an `Origin: https://www.twitch.tv` header. Their backend checks this against a whitelist and responds with `Access-Control-Allow-Origin: https://www.twitch.tv`. Since your domain isn't in their whitelist, the browser prevents your code from reading the response.

---

## 4. Implementation Strategy for SanctuaryPlayer

To implement a spoiler-free custom player (e.g., using `hls.js`), you cannot be browser-only. You must use a **Server-Side Proxy**.

### Recommended Architecture
1.  **Backend (Flask/GCR):** Create a new route (e.g., `/get-twitch-playlist/{vod_id}`).
2.  **Server-to-Server Request:** Your Flask server performs the GQL and Usher requests. Twitch does not enforce CORS on server-side requests (since there is no browser).
3.  **CORS Header:** Your server returns the M3U8 content to your frontend, including the header `Access-Control-Allow-Origin: *`.
4.  **Frontend (hls.js):**
    *   The frontend calls your Flask API.
    *   It receives the M3U8 data.
    *   It passes the segment URLs directly to `hls.js`.
    *   Since the `.ts` segments on CloudFront are typically CORS-open, the browser will allow the video to stream.

### Alternative: Cloudflare Workers
A lightweight proxy can be built using a Cloudflare Worker to intercept the Usher/GQL calls and append the necessary CORS headers.
