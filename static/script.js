var player;
var doSomethingAfterDelayTimerIds = new Map();
var timerId;
 // During seek operations, we store the target time so that repeated seeks can offset relative to this rather than the current
 // video time, which can lag behind. This is reset to null once the seek has completed.
var seekTarget = null;
var lastUploadedPosition = null; // The last video position successfully uploaded to the sever (for syncing time across devices)
// The wall clock time when the video first started playing. Used to hide spoilers (see usages).
var firstPlayTime = null;

var videoPlatform = null; // 'twitch' or 'youtube'
var isTwitch = false;
var isYoutube = false;

const youtubeVideoIdRegex = /\b[A-Za-z0-9-_]{11}\b/; // 11 alphanumeric chars (plus some special chars)
const twitchVideoIdRegex = /\b\d{10}\b/; // 10 numbers

//TODO: hide twitch "play" icon that appears while loading?
//TODO: youtube - loading video with a time set and then pressing play results in the time at the bottom briefly jumping to 0 before jumping to the correct time
//TODO: Twitch - on first load it shows a weird quarter-size frame in the corner before playing the video
//TODO: Twitch - quality support. Don't think we can do this for YouTube, but can show the current quality?
//TODO: Twitch - seeking the video shows the video length and title - show the blockers when seeking!
//TODO: improve display of recent videos in menu. Maybe show the video ID too, to distinguish between different videos with the same censored title (_ vs _)


function decodeFriendlyTimeString(timeStr) {
    // Decode strings of the format:
    //   1234
    //   1.45
    //   4567s
    //   1m2s
    //   2h1m40s
    //   2m
    //   2m1.5s
    //TODO: 8h20 - should go to 8:20:00, but currently goes to 8:00:20
    var timeMatch = timeStr.match(/^(?:(\d+)h)?(?:(\d+)m)?(?:([\d.]+)s?)?$/);
    if (!timeMatch) {
        return null;
    }
    var h = parseInt(timeMatch[1] || '0');
    var m = parseInt(timeMatch[2] || '0');
    var s = parseFloat(timeMatch[3] || '0');
    return 3600 * h + 60 * m + s;
}

function toFriendlyTimeString(seconds) {
    var hours = Math.floor(seconds / 3600);
    var minutes = Math.floor((seconds % 3600) / 60);
    var seconds = Math.floor(seconds % 60);

    return hours + 'h' + minutes.toString().padStart(2, '0') + 'm' + seconds.toString().padStart(2, '0') + 's';
}

function toFriendlyTimeStringColons(seconds) {
    var hours = Math.floor(seconds / 3600);
    var minutes = Math.floor((seconds % 3600) / 60);
    var seconds = Math.floor(seconds % 60);

    return hours + ':' + minutes.toString().padStart(2, '0') + ':' + seconds.toString().padStart(2, '0');
}

function onMenuButtonClick(e) {
    if (window.getComputedStyle(document.getElementById("menu")).display == "block") {
        // Menu already open - close it
        document.getElementById("menu").style.display = "none";
    }
    else {
        // Menu not already open - open it

        // Pause video once menu opens, as on mobile the menu covers the video so you wouldn't want it to keep playing
        if (player) {
            pause();
        }

        // Update the list of saved positions each time the menu is opened, so that it is up-to-date (otherwise would need to refresh the page)
        fetchSavedPositions();

        document.getElementById("menu").style.display = "block";
    }
}

// Although we can also get the video ID from the URL, this might give us a more 'canonical' version
function getVideoIdFromPlayer() {
    if (isYoutube) {
        return player.getVideoData().video_id;
    } else if (isTwitch) {
        return player.getVideo();
    }
}

function isVideoLoadedSuccessfully() {
    // Both YouTube and Twitch will only report the video ID once it has been loaded
    return !!getVideoIdFromPlayer();
}

function getAvailablePlaybackRates() {
    if (isYoutube) {
        return player.getAvailablePlaybackRates();
    } else {
        return [ "1" ]; // Twitch doesn't seem to support playback rates
    }
}

function getCurrentPlaybackRate() {
    if (isYoutube) {
        return player.getPlaybackRate();
    } else {
        return "1"; // Twitch doesn't seem to support playback rates
    }
}

function isPlaying() {
    if (!isVideoLoadedSuccessfully()) {
        return false;
    }
    if (isYoutube) {
        return player.getPlayerState() == YT.PlayerState.PLAYING;
    } else if (isTwitch) {
        return player.getPlayerState().playback == "Playing";
    }
}

function isPaused() {
    if (!isVideoLoadedSuccessfully()) {
        return false;
    }
    if (isYoutube) {
        return player.getPlayerState() == YT.PlayerState.PAUSED;
    } else if (isTwitch) {
        return player.isPaused();
    }
}

function isEnded() {
    if (!isVideoLoadedSuccessfully()) {
        return false;
    }
    if (isYoutube) {
        return player.getPlayerState() == YT.PlayerState.ENDED;
    } else if (isTwitch) {
        return player.getEnded();
    }
}

function isSignedIn() {
    return localStorage.getItem("user_id") != null && localStorage.getItem("user_id") != "";
}

function updateSignedInStatus() {
    document.getElementById("sign-in-button").style.display = isSignedIn() ? "none" : "block";
    document.getElementById("sign-out-button").style.display = isSignedIn() ? "block" : "none";
    if (isSignedIn()) {
        document.getElementById("sign-out-button").innerText = "Sign out (" + localStorage.getItem("user_id") + "/" + localStorage.getItem("device_id") + ")...";
    }
}

function signIn() {
    var userId = prompt("Please enter a User ID. This can be anything you want. Enter the same User ID on a different device to sync. " +
        "Please be aware that this is not secure - if anybody else types the same User ID, they will have access to your synced data!",
        localStorage.getItem("user_id") ?? "");
    if (userId) {
        // User entered a non-empty string - sign in
        var deviceId = prompt("Please enter a Device ID. This can be anything you want. This is used to distinguish this device from any others you sign in on.",
                              localStorage.getItem("device_id") ?? "Device 1");
        if (deviceId)
        {
            localStorage.setItem("user_id", userId);
            localStorage.setItem("device_id", deviceId);

            updateSignedInStatus();

            // Immediately fetch the list of saved positions, in case the user already has data saved from a different device, this will show
            // it immediately rather than only when they reload the page
            fetchSavedPositions();

            // This will make the position saved again immediately, even if we just saved it with a different account/device
            lastUploadedPosition = null;
        }
    }
}

function signOut() {
    if (confirm("Are you sure you want to sign out? You may want to remember your user ID and device ID:\n" + localStorage.getItem("user_id") + "\n" + localStorage.getItem("device_id"))) {
        localStorage.removeItem("user_id");
        localStorage.removeItem("device_id");

        // Rather than updating the UI now, reloading the page is an easy way to guarantee everything is up-to-date
        window.location.reload();
    }
}

function fetchSavedPositions() {
    if (isSignedIn())
    {
        var params = new URLSearchParams({
            'user_id': localStorage.getItem("user_id"),
            'device_id': localStorage.getItem("device_id"),
        });
        // If a video has already been selected, include this in the request, so that we also get data about most recent positions for this particular video
        // Note that we get this from the URL rather than the YouTube player object, because that might not yet have been initialised.
        var urlParams = new URLSearchParams(window.location.search);
        if (urlParams.has('videoId')) {
            params.append("current_video_id", urlParams.get("videoId"));
        }

        document.getElementById("saved-positions-other-videos-loading").style.display = "inline-block";
        document.getElementById("saved-positions-this-video-loading").style.display = "inline-block";
        fetch("get-saved-positions?" + params.toString())
            .then(response => {
                if (!response.ok) {
                    throw new Error('get-saved-positions response was not OK');
                }
                return response.json();
            })
            .then(response => {
                function createButton(x, showVideo) {
                    var params = new URLSearchParams(window.location.search);
                    params.set('videoId', x.video_id);
                    params.set('time', x.position);

                    var button = document.createElement("button");
                    button.addEventListener('click', function() {
                        window.location = '?' + params.toString();
                    });
                    button.innerText = showVideo ?
                        x.device_id + ": " + (x.video_title || x.video_id) + " at " + toFriendlyTimeStringColons(x.position) :
                        x.device_id + ": " + toFriendlyTimeStringColons(x.position);
                    return button;
                }

                while (document.getElementById("saved-positions-other-videos").lastElementChild?.tagName == "BUTTON") {
                    document.getElementById("saved-positions-other-videos").lastElementChild.remove();
                }
                document.getElementById("saved-positions-other-videos-header").style.display = response.other_videos.length > 0 ? "block" : "none";
                for (var x of response.other_videos) {
                    var opt = createButton(x, true);
                    document.getElementById("saved-positions-other-videos").appendChild(opt);
                }
                while (document.getElementById("saved-positions-this-video").lastElementChild?.tagName == "BUTTON") {
                    document.getElementById("saved-positions-this-video").lastElementChild.remove();
                }
                document.getElementById("saved-positions-this-video-header").style.display = response.this_video && response.this_video.length > 0 ? "block" : "none";
                if (response.this_video)
                {
                    for (var x of response.this_video) {
                        var opt = createButton(x, false);
                        document.getElementById("saved-positions-this-video").appendChild(opt);
                    }
                }
            })
            .catch((error) => {
                console.error('Error getting positions from server:', error);
            })
            .finally(() => {
                document.getElementById("saved-positions-other-videos-loading").style.display = "none";
                document.getElementById("saved-positions-this-video-loading").style.display = "none";
            });
    }
}

function changeVideo() {
    var userValue = prompt("Please enter YouTube video URL or video ID:");
    if (!userValue) {
        return;
    }

    // Extract video ID and time from URL/string. Needs to work for:
    //   3fgD9k8Hkbc
    //   https://youtu.be/3fgD9k8Hkbc
    //   https://youtu.be/3fgD9k8Hkbc?t=3839
    //   https://www.youtube.com/watch?v=3fgD9k8Hkbc
    //   https://www.youtube.com/watch?v=3fgD9k8Hkbc&t=54m39s
    //   https://www.youtube.com/watch?v=3fgD9k8Hkbc&t=54m39s&bob=someting
    //   https://youtube.com/watch?v=jDqFz0ZzoZo&si=3b7duJGqF-SKQqKI
    var videoId = null;
    var time = null;

    //TODO: Twitch URLs/video IDs

    // First try parsing it as a URL
    url = URL.parse(userValue);
    if (url) {
        // The video ID could be the last part of the path, or it could be a query string paramater
        videoId = url.searchParams.get("v");
        if (videoId == null) {
            var lastPart = url.pathname.split("/").pop();
            var match = lastPart.match(youtubeVideoIdRegex);
            if (match) {
                videoId = match[0];
            }
        }

        // time is always a query string parameter
        time = url.searchParams.get("t");
    } else {
        // Fallback to a simpler regex search, just to get the video ID (no time)
        var match = userValue.match(youtubeVideoIdRegex);
        if (match) {
            videoId = match[0];
        }
        time = null;
    }

    if (videoId == null) {
        alert("Unknown URL/video ID format")
        return;
    }

    // Modify the current page's URL with the new video/time
    var params = new URLSearchParams(window.location.search);
    params.set('videoId', videoId);
    if (time != null) {
        params.set('time', time);
    } else {
        params.delete('time'); // Important to overwrite any existing time value, so it doesn't start halfway through!
    }
    window.location = '?' + params.toString();
}

function onPlayerReady() {
    console.log("onPlayerReady");

    // Even though this is the 'on ready' callback, there might have been an error loading the video, for example
    // if the videoId was invalid. Check if the video metadata loaded properly, which seems to be a good indication for a
    // successful load
    if (isVideoLoadedSuccessfully()) {
        document.getElementById("loading-status").style.display = "none";
        document.title = getSafeTitle();
        // The blocker box at the top hides the video title, as it may contain spoilers, so we hide it, but show a filtered version of the title instead.
        document.getElementById("blocker-top").innerText = getSafeTitle();

        // Show the video controls, now that they will work
        document.getElementById("player-overlay-controls-mid").style.display = 'flex';
        document.getElementById("player-overlay-controls-bot").style.display = 'flex';

        for (var rate of getAvailablePlaybackRates()) {
            var opt = document.createElement("option");
            opt.value = rate;
            opt.text = rate + "x";
            document.getElementById("speed-select").options.add(opt);
        }
        document.getElementById("speed-select").value = getCurrentPlaybackRate();

        // Update UI immediately, rather than waiting for the first tick
        onTimer();

        // Start background refresh timer
        // Note we don't start it before now, otherwise the player.getCurrentTime() might return 0 and we don't want
        // to report that.
        timerId = window.setInterval(onTimer, 500);
    } else {
        document.getElementById("loading-status").innerText = 'Error!';
    }
}

// Note we use this in preference to checking the PlayerState, because when the video is paused and we seek, the player
// state remains at PAUSED, so we have no way to tell. Also the BUFFERING state could be triggered by other things
// (e.g. slow internet)
function isSeeking() {
    return seekTarget != null;
}

function getEffectiveCurrentTime() {
    return isSeeking() ? seekTarget : player.getCurrentTime(); // Same API for both YouTube and Twitch!
}

// Seeks to the given time, updating our own seekTarget variable too
function seekTo(target) {
    // Clamp to valid values, to prevent weird display of seek times (negative, or never gets to the target time)
    if (target < 0) {
        target = 0;
    }
    if (player.getDuration() && target > player.getDuration()) { // getDuration may return 0 if not available, according to docs. It's the same API for Twitch and YouTube.
        target = player.getDuration();
    }

    console.log("Seeking to " + target);

    seekTarget = target;
    if (isYoutube) {
        player.seekTo(target);
    } else if (isTwitch) {
        player.seek(target);
    }

    onTimer(); // Update UI
}

function seekRelative(offset) {
    var base = getEffectiveCurrentTime();
    var target = base + offset;
    seekTo(target);
}

// Gets the video title, filtering out potential spoilers
function getSafeTitle() {
    var title = null;
    if (isYoutube) {
        title = player.getVideoData().title;
    } else if (isTwitch) {
        title = player && player.getVideo(); //TODO: find a way to get the title, just using ID for now
    }

    // The video title may have something like "X vs Y Game 5", which tells you it goes to game 5, so hide this
    var r = /Game \d+/ig;
    title = title.replace(r, "Game _");

    // Team names in title (e.g. EG vs LLL) can ruin the teams playing in the last game of the day which could be a spoiler
    // if it's tiebreaker that may or may not happen depending on other games. (if the title is updated throughout live video's
    // original broadcast, then the title we see afterwards is the final title).
    // Examples:
    // GAM vs. DRX | Worlds Game 3
    // GAM vs. DRX
    // Team 1 vs. Team 2
    // Team OGOBAG vs tramula big face | Champions match
    // Today | A A vs B B | Now
    // Nothing to see here!
    // This is a game between A vs B
    // BAG v TAG
    // Not gonna match chavs is it
    var r = /\b[^|\n]+\bvs?\b[^|\n]+\b/ig;
    title = title.replace(r, "_ vs _");

    return title;
}

function onYoutubePlayerStateChange(event) {
    switch (event.data) {
        case YT.PlayerState.UNSTARTED: onPlayerStateChange("unstarted"); break;
        case YT.PlayerState.ENDED: onPlayerStateChange("ended"); break;
        case YT.PlayerState.PLAYING: onPlayerStateChange("playing"); break;
        case YT.PlayerState.PAUSED: onPlayerStateChange("paused"); break;
        case YT.PlayerState.BUFFERING: onPlayerStateChange("buffering"); break;
        case YT.PlayerState.CUED: onPlayerStateChange("cued"); break;
        default: console.log("Unknown YouTube player state: " + event.data); break;
    }
}

function onPlayerStateChange(newStateStr) {
    // Note that during this function, directly querying the player's state will behave differently on YouTube vs Twitch:
    //   * For YouTube, it retrieves the new state
    //   * For Twitch, it retrieves the old state
    // Therefore it's best to use the newStateStr parameter where possible
    console.log("onPlayerStateChange: " + newStateStr);

    // Toggle visibility of blocker box to hide related videos bar at bottom, which can spoil future games.
    // Also hide the video title, as it may have something like "X vs Y Game 5", which tells you it goes to game 5
    if (newStateStr == "paused") {
        // Even though we show the pause blockers just before pausing the video, we also do it here just in case the video
        // gets paused through other means (not initiated by us)
        showPauseBlockers();
        document.getElementById('play-pause-button').style.backgroundImage = "url('static/play.png')";
    }
    else if (newStateStr == "playing") {
        if (isYoutube && !firstPlayTime) {
            // There appears to be a bug when a specific start time is requested, where the player will sometimes jump to a different time
            // in the video when first playing. I think this might be a 'feature' where the player remembers where you were in the video
            // and tries to restore it, but it gets in our way. This is a pretty hacky check for this bug which forces a re-seek.
            if (isSeeking() && Math.abs(player.getCurrentTime() - seekTarget) > 10)
            {
                console.log("Re-seeking to start time to workaround bug (player time: " + player.getCurrentTime() + ", seekTarget: " + seekTarget);
                player.seekTo(seekTarget);
            }

            firstPlayTime = performance.now();
        }

        document.getElementById('play-pause-button').style.backgroundImage = "url('static/pause.png')";

        // Hide the blocker boxes after a short delay, as the UI elements that we're covering take a little time to disappear

        // Bottom blocker - related videos bar (YouTube) or video length (Twitch)
        var delay = isYoutube ? 250 : isTwitch ? 5500 : 0;  // Twitch takes longer to hide the video length bar
        doSomethingAfterDelay("hideBottomBlocker", delay, function() {
            // Make sure video hasn't been paused again during the timer
            if (isPlaying()) {
                document.getElementById('blocker-bottom').style.display = 'none';
            }
        });

        // Full blocker - related videos shown at end-of-video
        doSomethingAfterDelay("hideFullBlocker", 250, function () {
            // Make sure video hasn't been paused again during the timer
            if (isPlaying()) {
                document.getElementById('blocker-full').style.display = 'none';
            }
        });

        // Top blocker - video title
        // Hide the title blocker after a _longer_ delay, if the video was recently started. The player displays the title
        // for a few seconds after first playing it, so we need to keep our blocker for longer in this case.
        var delay = isYoutube ? ((performance.now() - firstPlayTime < 5000) ? 5000 : 250) : isTwitch ? 5500 : 0;
        doSomethingAfterDelay("hideTopBlocker", delay, function () {
            // Make sure video hasn't been paused again during the timer
            if (isPlaying()) {
                document.getElementById('blocker-top').style.display = 'none';
            }
        });
    }
    else if (newStateStr == "ended") {
        // Hide related videos that fill the player area at the end of the video
        document.getElementById('blocker-full').style.display = 'block';
        document.getElementById('play-pause-button').style.backgroundImage = "url('static/play.png')";

        if (isTwitch) {
            player.setVideo("0"); // Stop the player from auto-loading the next video (random hack that seems to work quite well)
        }
    }

    // Keep the UI responsive to changes in player state (rather than waiting for the next tick)
    onTimer();
}

function onPlaybackQualityChange(event)
{
    console.log("onPlaybackQualityChange: " + event.data);
    // document.getElementById("quality-text").innerText = "Quality: " + event.data;
}

function onPlaybackRateChange(event)
{
    console.log("onPlaybackRateChange: " + event.data);
    document.getElementById("speed-select").value = player.getPlaybackRate();
}

function onError(event)
{
    console.log("onError" + event.data);
}

function hideControlsShortly() {
    doSomethingAfterDelay("hideControls", 2000, function() {
        // Don't hide the controls if the menu is open
        if (window.getComputedStyle(document.getElementById("menu")).display == "none") {
            document.getElementById("player-overlay-controls").style.display = 'none';
        }
    });
}

// Helper function to run a callback after a delay, but also handles
// cancelling any previous request with the same unique name.
// This is useful for things like hiding the controls after a delay, where we want to reset the timer each time the user interacts with the video.
function doSomethingAfterDelay(uniqueName, delay, callback) {
    timerId = doSomethingAfterDelayTimerIds.get(uniqueName);
    if (timerId != undefined) {
        window.clearTimeout(timerId);
    }
    timerId = window.setTimeout(callback, delay);
    doSomethingAfterDelayTimerIds.set(uniqueName, timerId);
}

function onOverlayClick(event) {
    // Don't do anything until the video has actually loaded though, as the Menu button needs to be easy to find
    if (!isVideoLoadedSuccessfully()) {
        return;
    }

    // Clicking anywhere on the video (which has our overlay on top) -> show the controls
    document.getElementById("player-overlay-controls").style.display = 'flex';
    hideControlsShortly();
}

function onOverlayControlsClick(event) {
    // Don't do anything until the video has actually loaded though, as the Menu button needs to be easy to find
    if (!isVideoLoadedSuccessfully()) {
        return;
    }

    // Clicking on background of the controls div (but not on any individual control) -> hide the controls.
    if (event.target == document.getElementById("player-overlay-controls")) {
        document.getElementById("player-overlay-controls").style.display = 'none';
        // Also close the menu if it's open, so it isn't open the next time the user brings up the controls
        document.getElementById("menu").style.display = 'none';
        event.stopPropagation();  // Stop the click from going up to the player-overlay, which would show the controls again!
    }

    // Clicking anywhere on the controls (e.g. any button) - refresh the timeout to hide the controls
    hideControlsShortly();
}

function togglePlayPause() {
    if (isPlaying()) {
        pause();
    } else {
        play();
    }
}

function play() {
    if (isYoutube) {
        player.playVideo();
    } else if (isTwitch) {
        player.play();
    }
}

function pause() {
    // Before telling the player to pause, show the blockers. This ensures that they are visible before the player shows the title etc.,
    // so we don't catch a frame that might contain a spoiler.
    showPauseBlockers();

    if (isYoutube) {
        player.pauseVideo();
    } else if (isTwitch) {
        player.pause();
    }
}

function showPauseBlockers() {
    document.getElementById('blocker-bottom').style.display = 'block';
    document.getElementById('blocker-top').style.display = 'block';
}

function toggleFullscreen(event) {
    if (document.fullscreenElement) {
        document.exitFullscreen();
    }
    else {
        // Make the whole page fullscreen, so the overlay still works
        document.getElementById("root").requestFullscreen();
    }
}

function seekButtonClicked(e) {
    var amount = parseInt(this.dataset["amount"]);
    seekRelative(amount);
}

function changeTime() {
    if (player) {
        pause();
    }

    // Show the prompt on a timeout, so that the pause command above has time to take effect.
    // The timeout must be >0, as we need to make sure our onPlayerStateChange callback happens
    // first to show the blocker.
    window.setTimeout(function () {
        var newTimeStr = prompt("Please enter new time:", toFriendlyTimeString(getEffectiveCurrentTime()));
        if (!newTimeStr) {
            return;
        }
        var time = decodeFriendlyTimeString(newTimeStr);
        if (time != null) {
            seekTo(time);
        }
    }, 100);
}

function onTimer() {
    var effectiveCurrentTime = getEffectiveCurrentTime();

    document.getElementById("current-time-span").innerText = toFriendlyTimeStringColons(effectiveCurrentTime);
    document.getElementById("current-time-span").style.backgroundColor = isSeeking() ? 'orange' : 'white';

    // Clear seek time if we were seeking and have now reached the target.
    // We allow a little leeway as there the player might not seek to exactly the time we requested
    // The Twitch player seems to be quite laggy with reporting time updates, so even after the video has visually
    // finished seeking and the player state change callback is fired, the getCurrentTime() still reports the old value,
    // so we can't check this in onPlayerStateChange() like we used to.
    if (isSeeking() && Math.abs(player.getCurrentTime() - seekTarget) < 2.0)
    {
        seekTarget = null;
    }

    // Update URL to reflect the current time in the video, so refreshing the page (or closing and re-opening
    // the browser will resume the video at the current time).
    // This doesn't behave quite like we want with the Chrome global history though (it has one entry per timestamp!).
    // See this firefox bug report for some discussion: https://bugzilla.mozilla.org/show_bug.cgi?id=753264
    var params = new URLSearchParams(window.location.search);
    params.set('time', toFriendlyTimeString(effectiveCurrentTime));
    newState = params.toString();
    // Only call the API if the position has changed since last time. This avoids unecessary calls, for example when the video is paused
    if (window.history.state != newState)
    {
        window.history.replaceState(newState, '', '?' + params.toString());
    }

    // If the user is signed in, also upload the current position to the web server, so that it can be synced with other devices
    if (isSignedIn())
    {
        // Don't upload too often to avoid overloading the server. Especially if we're paused!
        if (lastUploadedPosition == null || Math.abs(effectiveCurrentTime - lastUploadedPosition) > 10.0) {
            var params = new URLSearchParams({
                'user_id': localStorage.getItem("user_id"),
                'device_id': localStorage.getItem("device_id"),
                'video_id': getVideoIdFromPlayer(),
                'video_title': getSafeTitle(),
                'position': Math.round(effectiveCurrentTime), // Server saves whole numbers only
            });
            fetch('save-position?' + params.toString(), { method: 'POST'})
                .then(response => {
                    if (!response.ok) {
                        throw new Error('save-position response was not OK');
                    }
                    lastUploadedPosition = params.get('position') // Note we use this rather than effectiveCurrentTime, which might have advanced in the meantime (as this is an async callback)
                })
                .catch((error) => {
                    console.error('Error saving position to server:', error);
                });
        }
    }
}

// This function is called automatically when the youtube iframe script API loads.
// Note that this is only called if a video needs to be loaded (i.e. the URL has a video ID).
function onYouTubeIframeAPIReady() {
    console.log("onYouTubeIframeAPIReady");
    onAPIReady();
}

// This function is called when the YouTube or Twitch API script loads
// Note that this is only called if a video needs to be loaded (i.e. the URL has a video ID).
function onAPIReady() {
    // Load video
    var params = new URLSearchParams(window.location.search);
    var startTime = 0;
    if (params.has('time')) {
        startTime = decodeFriendlyTimeString(params.get('time'));
    }
    // There seem to be issues with requesting a small start time != 0, especially for live streams. So put a lower limit on it.
    // Also for live stream videos, 0 seems to be interpreted as starting from live, which could be a spoiler.
    if (isYoutube && startTime < 10) {
        startTime = 10;
    }
    startTime = Math.round(startTime); // Fractional numbers are invalid and won't work!

    seekTarget = startTime; // Treat the start time as a seek target, so the UI shows this time rather than 0 when loading

    if (isYoutube) {
        player = new YT.Player('player', {
            height: '100%',
            width: '100%',
            videoId: params.get('videoId'),
            playerVars: {
                'controls': 0,
                // Note that if 'start' is not set (or set to something invalid), then YouTube has some kind of memory where it tries
                // to start where you last were. We don't want this, as we handle it ourselves, so make sure to always set this, even if no start time was provided (so we default to the beginning)
                'start': startTime,
                // Disable fullscreen - we'll handle this ourselves so that we're able to display our own stuff
                // over the top of the fullscreen video, and be aware when the video toggles fullscreen (which
                // it seems we can't if the iframe document itself triggers it)
                'fs': 0,
                // Turn off autoplay because a) it doesn't work for mobile so it's inconsistent and b) can be annoying especially if tab reloads in the background
                'autoplay': 0,
            },
            events: {
                'onReady': onPlayerReady,
                'onStateChange': onYoutubePlayerStateChange,
                'onPlaybackQualityChange': onPlaybackQualityChange,
                'onPlaybackRateChange': onPlaybackRateChange,
                'onError': onError,
            }
        });
    } else if (isTwitch) {
        var params = new URLSearchParams(window.location.search);
        var options = {
            height: '100%',
            width: '100%',
            video: params.get('videoId'),
            time: startTime,
            // Turn off autoplay because a) it doesn't work for mobile so it's inconsistent and b) can be annoying especially if tab reloads in the background
            autoplay: false,
        };
        player = new Twitch.Player("player", options);
        // List of Twich Player events (copied from the Twitch API javascript source):
        //    AUTHENTICATE
        //    CAPTIONS
        //    ENDED
        //    ERROR
        //    OFFLINE
        //    ONLINE
        //    PAUSE
        //    PLAY
        //    PLAYBACK_BLOCKED
        //    PLAYING
        //    VIDEO_PAUSE
        //    VIDEO_PLAY
        //    VIDEO_READY
        //    READY
        //    SEEK
        //  List of errors:
        //      ABORTED
        //      NETWORK
        //      DECODE
        //      FORMAT_NOT_SUPPORTED
        //      CONTENT_NOT_AVAILABLE
        //      RENDERER_NOT_AVAILABLE
        player.addEventListener(Twitch.Player.VIDEO_READY, onPlayerReady);
        player.addEventListener(Twitch.Player.VIDEO_PLAY, function() { onPlayerStateChange("playing"); } );
        player.addEventListener(Twitch.Player.VIDEO_PAUSE, function() { onPlayerStateChange("paused"); });
        player.addEventListener(Twitch.Player.ENDED, function() { onPlayerStateChange("ended"); });
        //TODO: if error loading video, e.g. video ID invalid, then we need to catch this. The below thing doesn't seem to work!
        player.addEventListener(Twitch.Player.ERROR, function(x) { console.log("Twitch error: " + x); });
        //TODO: better handling of ENDED - if ends naturally then the blocker doesn't always appear

        // also, if refresh the video when it's at the end, the blocker doesn't appear either!
        // also seeking away from the end of the video seems a bit broken
        //TODO: find a way to stop twitch from auto-loading the 'next' video after getting to the end
    }

    // Display blockers before the video loads so that when the video loads but hasn't started playing,
    // the title and related videos (YouTube) and video length bar (Twitch) are hidden
    showPauseBlockers();
}

function onKeyDown(event) {
    // event.preventDefault stops default browser behaviour, for example space bar from pressing the focused button
    switch (event.code) {
        case 'ArrowLeft':
            seekRelative(-5);
            event.preventDefault();
            break;
        case 'ArrowRight':
            seekRelative(+5);
            event.preventDefault();
            break;
        case 'KeyF':
            toggleFullscreen();
            event.preventDefault();
            break;
        case 'Space':
            togglePlayPause();
            event.preventDefault();
            break;
        case 'ArrowUp':
            var rates = getAvailablePlaybackRates();
            var currentIdx = rates.indexOf(getCurrentPlaybackRate());
            if (currentIdx < rates.length - 1) {
                player.setPlaybackRate(rates[currentIdx + 1]);
            }
            break;
        case 'ArrowDown':
            var rates = getAvailablePlaybackRates();
            var currentIdx = rates.indexOf(getCurrentPlaybackRate());
            if (currentIdx > 0) {
                player.setPlaybackRate(rates[currentIdx - 1]);
            }
            event.preventDefault();
            break;
    }
}

function onSpeedSelectChange(event) {
    player.setPlaybackRate(parseFloat(this.value));
}

function startup() {
    // Hookup event listeners
    document.getElementById("menu-button").addEventListener("click", onMenuButtonClick);
    document.getElementById("change-video-button").addEventListener("click", changeVideo);
    document.getElementById("sign-in-button").addEventListener("click", signIn);
    document.getElementById("sign-out-button").addEventListener("click", signOut);
    document.getElementById("current-time-span").addEventListener("click", changeTime);
    document.getElementById("player-overlay").addEventListener("click", onOverlayClick);
    document.getElementById("player-overlay-controls").addEventListener("click", onOverlayControlsClick);
    document.getElementById("play-pause-button").addEventListener("click", togglePlayPause);
    document.getElementById("fullscreen-button").addEventListener("click", toggleFullscreen);
    document.getElementById("speed-select").addEventListener("change", onSpeedSelectChange);
    document.addEventListener("keydown", onKeyDown)
    for (let button of document.getElementsByClassName("seek-button")) {
        button.addEventListener("click", seekButtonClicked);
    }

    var params = new URLSearchParams(window.location.search);
    if (params.has('videoId')) {
        // Detect if the video ID is for Twitch or YouTube
        if (params.get('videoId').match(youtubeVideoIdRegex)) {
            videoPlatform = 'youtube';
            isYoutube = true;

            // Configure YouTube-specific settings
            document.getElementById('blocker-top').style.height = "50px"; // Needs to be taller than the title bar
            document.getElementById('blocker-bottom').style.height = "50%"; // Needs to be taller than the related videos bar

            // Load the YouTube API script. Note I used to have this as a regular <script> element
            // in the HTML <head>, but this seemed to cause issues where the API wouldn't load correctly
            // about half the time (had to keep refreshing the page til it worked). This dynamic loading
            // code here comes from the reference page.
            // https://developers.google.com/youtube/iframe_api_reference
            var tag = document.createElement('script');
            tag.src = "https://www.youtube.com/iframe_api";
            var firstScriptTag = document.getElementsByTagName('script')[0];
            firstScriptTag.parentNode.insertBefore(tag, firstScriptTag);
            // The video itself will be loaded once the youtube API loads (it calls our onYouTubeIframeAPIReady() function)
        } else if (params.get('videoId').match(twitchVideoIdRegex)) {
            videoPlatform = 'twitch';
            isTwitch = true;

            // Configure Twitch-specific settings
            document.getElementById('blocker-top').style.height = "200px"; // Needs to be taller than the title bar
            document.getElementById('blocker-bottom').style.height = "100px"; // Needs to be taller than the video length bar, but not as tall as for YouTube

            var script = document.createElement('script');
            script.src = "https://player.twitch.tv/js/embed/v1.js";
            script.onload = onAPIReady;
            document.body.appendChild(script)
        } else {
            console.log("Unknown video platform!");
        }

        // Show start time at bottom of controls, as it may take a few seconds for the video to load
        // and we'd like the start time to be visible before then
        if (params.has('time')) {
            document.getElementById("loading-status").innerText = `Loading ${videoPlatform} video ${params.get('videoId')} at ${params.get('time')}...`;
        } else  {
            document.getElementById("loading-status").innerText = `Loading ${videoPlatform} video ${params.get('videoId')}...`;
        }
    }
    updateSignedInStatus();

    // If the user is already signed in, update the list of saved positions
    // Even though we update this when the user opens the menu, we also do it now because otherwise the size of the <select>
    // dropdown doesn't adjust properly once it's already open (you get a scrollbar which isn't so nice)
    fetchSavedPositions();
}

startup();