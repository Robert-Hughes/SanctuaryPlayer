var player;
var overlayControlsTimeoutId;
var timerId;

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

function changeVideo() {
    var url = prompt("Video URL");
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
            params.set('time', '0s'); // Important to overwrite any existing time value   
        }
    }
    else {
        alert("Invalid URL")
        return;
    }

    window.location = '?' + params.toString();
}

function onPlayerReady() {
    console.log("onPlayerReady");

    for (var rate of player.getAvailablePlaybackRates()) {
        var opt = document.createElement("option");
        opt.value = rate;
        opt.text = rate + "x";
        document.getElementById("speed-select").options.add(opt);
    }
    document.getElementById("speed-select").value = player.getPlaybackRate();
}

function onPlayerStateChange(event) {
    console.log("onPlayerStateChange: " + event.data);
    // Toggle visibility of blocker box to hide related videos bar at bottom, which can spoil future games.
    if (event.data == YT.PlayerState.PAUSED) {
        //TODO: also display this once the video has finished, as related videos appear at the end too?
        document.getElementById('blocker-box').style.display = 'block';
        document.getElementById('play-pause-button').style.backgroundImage = "url('play.png')";
    }
    else if (event.data == YT.PlayerState.PLAYING) {
        // Start background refresh timer if this is the first time the video has been played.
        // Don't start it before now, otherwise the player.getCurrentTime() might return 0 and we don't want
        // to report that.
        if (timerId == null) {
            window.setInterval(onTimer, 500);           
            document.getElementById("loading-status").style.display = 'none';
        }

        document.getElementById('play-pause-button').style.backgroundImage = "url('pause.png')";
        // Hide after a short delay, as it takes a short time for the related videos bar to disappear
        window.setTimeout(function () {
            // Make sure video hasn't been paused again during the timer
            if (player.getPlayerState() == YT.PlayerState.PLAYING) {
                document.getElementById('blocker-box').style.display = 'none';
            }
        }, 250);
    }
}

function onPlaybackQualityChange(event)
{
    console.log("onPlaybackQualityChange: " + event.data);
    document.getElementById("quality-text").innerText = "Quality: " + event.data;
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
    player.seekTo(player.getCurrentTime() + amount);
}

function changeTime() {
    var newTimeStr = prompt("New time:", toFriendlyTimeString(player.getCurrentTime()));
    if (!newTimeStr) {
        return;
    }
    var time = decodeFriendlyTimeString(newTimeStr);
    if (time != null) {
        player.seekTo(time);
    }
}

function onTimer() {
    if (player && player.getPlayerState() != YT.PlayerState.UNSTARTED)  // Video may not yet have been loaded
    {
        document.getElementById("current-time-span").innerText = toFriendlyTimeString(player.getCurrentTime());

        // Update URL to reflect the current time in the video, so refreshing the page (or closing and re-opening
        // the browser will resume the video at the current time).
        var params = new URLSearchParams(window.location.search);
        params.set('videoId', player.getVideoData().video_id);
        params.set('time', toFriendlyTimeString(player.getCurrentTime()));
        window.history.replaceState({}, '', '?' + params.toString());

        document.title = player.getVideoData().title;
    }
}

// This function is called automatically when the youtube iframe script API loads
// (see the <script> element on the HTML page).
function onYouTubeIframeAPIReady() {
    // Load video immediately if provided in URL
    var params = new URLSearchParams(window.location.search);
    if (params.has('videoId')) {
        var startTime = 0;
        if (params.has('time')) {
            startTime = decodeFriendlyTimeString(params.get('time'));
        }

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

function onKeyUp(event) {
    switch (event.code) {
        case 'ArrowLeft':
            player.seekTo(player.getCurrentTime() - 5);
            break;
        case 'ArrowRight':
            player.seekTo(player.getCurrentTime() + 5);
            break;
        case 'KeyF':
            toggleFullscreen();
            break;
        case 'Space':
            togglePlayPause();
            break;
    }
    event.preventDefault();  // Prevent (for example), space bar from pressing the focused button
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
document.addEventListener("keyup", onKeyUp)
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
// Main logic begins once youtube API loads (onYouTubeIframeAPIReady)