var player;
var overlayControlsTimeoutId;
var timerId;
 // During seek operations, we store the target time so that repeated seeks can offset relative to this rather than the current
 // video time, which can lag behind. This is reset to null once the seek has completed.
var seekTarget = null; 

function decodeFriendlyTimeString(timeStr) {
    // Decode strings of the format:
    //   1234
    //   4567s
    //   1m2s
    //   2h1m40s
    //   2m
    var timeMatch = timeStr.match(/^(?:(\d+)h)?(?:(\d+)m)?(?:(\d+)s?)?$/);
    if (!timeMatch) {
        return null;
    }
    var h = parseInt(timeMatch[1] || '0');
    var m = parseInt(timeMatch[2] || '0');
    var s = parseInt(timeMatch[3] || '0');
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

function changeVideo() {
    if (player) {
        player.pauseVideo();
    }

    // Show the prompt on a timeout, so that the pause command above has time to take effect.
    // The timeout must be >0, as we need to make sure our onPlayerStateChange callback happens
    // first to show the blocker.
    window.setTimeout(function () {
        var url = prompt("Please enter YouTube video URL:");
        if (!url) {
            return;
        }

        // Regex to extract video ID and time. Needs to work for:
        //   https://youtu.be/3fgD9k8Hkbc
        //   https://youtu.be/3fgD9k8Hkbc?t=3839
        //   https://www.youtube.com/watch?v=3fgD9k8Hkbc
        //   https://www.youtube.com/watch?v=3fgD9k8Hkbc&t=54m39s
        //   https://www.youtube.com/watch?v=3fgD9k8Hkbc&t=54m39s&bob=someting
        var match = url.match(/(?:watch\?v=|youtu\.be\/)(.*?)(?:&|\?|$)(?:t=(.*?))?(?:&|$)/);
        var params = new URLSearchParams(window.location.search);
        if (match && match[1]) {
            params.set('videoId', match[1]);
            if (match[2]) {
                params.set('time', match[2]);
            } else {
                params.delete('time'); // Important to overwrite any existing time value
            }
        }
        else {
            alert("Invalid URL")
            return;
        }

        window.location = '?' + params.toString();
    }, 100);
}

function onPlayerReady() {
    console.log("onPlayerReady");

    // If autoplay doesn't work (e.g. on mobile), indicate to user that they have to click play (otherwise will keep saying "loading video...")
    document.getElementById("loading-status").innerText = 'Ready to play';
    document.title = player.getVideoData().title;

    for (var rate of player.getAvailablePlaybackRates()) {
        var opt = document.createElement("option");
        opt.value = rate;
        opt.text = rate + "x";
        document.getElementById("speed-select").options.add(opt);
    }
    document.getElementById("speed-select").value = player.getPlaybackRate();

    onTimer();
}

// Note we use this in preference to checking the PlayerState, because when the video is paused and we seek, the player
// state remains at PAUSED, so we have no way to tell. Also the BUFFERING state could be triggered by other things
// (e.g. slow internet)
function isSeeking() {
    return seekTarget != null;
}

function getEffectiveCurrentTime() {
    return isSeeking() ? seekTarget : player.getCurrentTime();
}

// Seeks to the given time, updating our own seekTarget variable too
function seekTo(target) {
    console.log("Seeking to " + target);

    seekTarget = target;
    player.seekTo(target);

    onTimer(); // Update UI
}

function seekRelative(offset) {
    var base = getEffectiveCurrentTime();
    var target = base + offset;
    seekTo(target);
}

function onPlayerStateChange(event) {
    console.log("onPlayerStateChange: " + event.data);
    // Toggle visibility of blocker box to hide related videos bar at bottom, which can spoil future games.
    if (event.data == YT.PlayerState.PAUSED) {
        document.getElementById('blocker-box').style.display = 'block';
        document.getElementById('blocker-box').style.height = '50%';
        document.getElementById('blocker-box').innerText = 'Video paused. Hiding related videos.';
        document.getElementById('play-pause-button').style.backgroundImage = "url('play.png')";
    }
    else if (event.data == YT.PlayerState.PLAYING) {
        // Start background refresh timer if this is the first time the video has been played.
        // Don't start it before now, otherwise the player.getCurrentTime() might return 0 and we don't want
        // to report that.
        if (timerId == null) {
            timerId = window.setInterval(onTimer, 500);           
            document.getElementById("loading-status").style.display = 'none';
        }

        // Clear seek time if we were seeking - the video is now playing so the seek must be finished.
        // This doesn't pick up the case where we seek while paused, but this isn't a big issue and we have no
        // good way to detect seek completion when paused other than checking the time, which could be error-prone.
        seekTarget = null;

        document.getElementById('play-pause-button').style.backgroundImage = "url('pause.png')";
        // Hide after a short delay, as it takes a short time for the related videos bar to disappear
        window.setTimeout(function () {
            // Make sure video hasn't been paused again during the timer
            if (player.getPlayerState() == YT.PlayerState.PLAYING) {
                document.getElementById('blocker-box').style.display = 'none';
            }
        }, 250);
    }
    else if (event.data == YT.PlayerState.ENDED) {
        // Hide related videos that fill the player area at the end of the video
        document.getElementById('blocker-box').style.display = 'block';
        document.getElementById('blocker-box').style.height = '100%';
        document.getElementById('blocker-box').innerText = 'End of video. Hiding related videos.';
        document.getElementById('play-pause-button').style.backgroundImage = "url('play.png')";
    }

    onTimer();
}

function onPlaybackQualityChange(event)
{
    console.log("onPlaybackQualityChange: " + event.data);
    // document.getElementById("quality-text").innerText = "Quality: " + event.data;
}

function onPlaybackRateChange(event)
{
    console.log("onPlaybackRateChange" + event.data);
    document.getElementById("speed-select").value = player.getPlaybackRate();
}

function onError(event)
{
    console.log("onError" + event.data);
}

function hideControlsShortly() {
    if (overlayControlsTimeoutId != null) {
        window.clearTimeout(overlayControlsTimeoutId);
    }
    overlayControlsTimeoutId = window.setTimeout(function () {
        document.getElementById("player-overlay-controls").style.display = 'none';
    }, 2000);
}

function onOverlayClick(event) {
    document.getElementById("player-overlay-controls").style.display = 'flex';
    hideControlsShortly();
}

function onOverlayControlsClick(event) {
    // Clicking on background of the controls -> hide the controls
    if (event.target == document.getElementById("player-overlay-controls")) {
        document.getElementById("player-overlay-controls").style.display = 'none';
        event.stopPropagation();  // Stop the click from going up to the player-overlay, which would show the controls again!
    }

    // Clicking anywhere on the controls (e.g. any button) - refresh the timeout to hide the controls
    hideControlsShortly();
}

function togglePlayPause(event) {
    if (player.getPlayerState() == YT.PlayerState.PLAYING) {
        player.pauseVideo();
    }
    else {
        player.playVideo();
    }
}

function toggleFullscreen(event) {
    if (document.fullscreenElement) {
        document.exitFullscreen();
    }
    else {
        // Make the whole mid-container fullscreen, so the overlay still works
        document.getElementById("mid-container").requestFullscreen();
    }
}

function seekButtonClicked(e) {
    var amount = parseInt(this.dataset["amount"]);
    seekRelative(amount);
}

function changeTime() {
    if (player) {
        player.pauseVideo();
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
    if (player && player.getPlayerState() != YT.PlayerState.UNSTARTED)  // Video may not yet have been loaded //TODO: sometimes the state is UNSTARTED after coming out of sleep? And this causes seeking to not show the target time correctly
    {
        var effectiveCurrentTime = getEffectiveCurrentTime();

        document.getElementById("current-time-span").innerText = toFriendlyTimeStringColons(effectiveCurrentTime);
        document.getElementById("current-time-span").style.backgroundColor = isSeeking() ? 'orange' : 'white';

        // Update URL to reflect the current time in the video, so refreshing the page (or closing and re-opening
        // the browser will resume the video at the current time).
        // This doesn't behave quite like we want with the Chrome global history though (it has one entry per timestamp!).
        // See this firefox bug report for some discussion: https://bugzilla.mozilla.org/show_bug.cgi?id=753264
        var params = new URLSearchParams(window.location.search);
        params.set('videoId', player.getVideoData().video_id);
        params.set('time', toFriendlyTimeString(effectiveCurrentTime));
        window.history.replaceState({}, '', '?' + params.toString());

        // Also upload the current position to the web server, so that it can be synced with other devices
        //TODO:
    }
}

// This function is called automatically when the youtube iframe script API loads
// (see the <script> element on the HTML page).
function onYouTubeIframeAPIReady() {
    console.log("onYouTubeIframeAPIReady");
    // Load video immediately if provided in URL
    var params = new URLSearchParams(window.location.search);
    if (params.has('videoId')) {
        // Default to a very short time into the video rather than 0, because for live stream videos, 0 seems to be interpreted as starting from
        // live, which could be a spoiler.
        var startTime = 2;  
        if (params.has('time')) {
            startTime = decodeFriendlyTimeString(params.get('time'));
        }
        seekTarget = startTime; // Treat the start time as a seek target, so the UI shows this time rather than 0 when loading

        player = new YT.Player('player', {
            height: '100%',
            width: '100%',
            videoId: params.get('videoId'),
            playerVars: {
                'controls': 0,
                'start': startTime,
                // Disable fullscreen - we'll handle this ourselves so that we're able to display our own stuff
                // over the top of the fullscreen video, and be aware when the video toggles fullscreen (which
                // it seems we can't if the iframe document itself triggers it)
                'fs': 0,
                // Note that autoplay doesn't work on mobile (a known limitation)
                'autoplay': 1,
            },
            events: {
                'onReady': onPlayerReady,
                'onStateChange': onPlayerStateChange,
                'onPlaybackQualityChange': onPlaybackQualityChange,
                'onPlaybackRateChange': onPlaybackRateChange,
                'onError': onError,
            }
        });
    }
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
    }
}

function onSpeedSelectChange(event) {
    player.setPlaybackRate(parseFloat(this.value));
}

// Hookup event listeners
document.getElementById("change-video-button").addEventListener("click", changeVideo);
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

// Show start time at bottom of controls, as it may take a few seconds for the video to load
// and we'd like the start time to be visible before then
var params = new URLSearchParams(window.location.search);
if (params.has('videoId')) {
    if (params.has('time')) {
        document.getElementById("loading-status").innerText = 'Loading video at: ' + params.get('time') + '...';
    } else  {
        document.getElementById("loading-status").innerText = 'Loading video...';
    }
}

// Load the YouTube API script. Note I used to have this as a regular <script> element
// in the HTML <head>, but this seemed to cause issues where the API wouldn't load correctly
// about half the time (had to keep refreshing the page til it worked). This dynamic loading
// code here comes from the reference page.
// https://developers.google.com/youtube/iframe_api_reference
var tag = document.createElement('script');
tag.src = "https://www.youtube.com/iframe_api";
var firstScriptTag = document.getElementsByTagName('script')[0];
firstScriptTag.parentNode.insertBefore(tag, firstScriptTag);

// Main logic begins once youtube API loads (it calls our onYouTubeIframeAPIReady() function)