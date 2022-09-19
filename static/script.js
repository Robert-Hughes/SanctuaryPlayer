var player;
var overlayControlsTimeoutId;
var timerId;
 // During seek operations, we store the target time so that repeated seeks can offset relative to this rather than the current
 // video time, which can lag behind. This is reset to null once the seek has completed.
var seekTarget = null; 
var lastUploadedPosition = null; // The last video position successfully uploaded to the sever (for syncing time across devices)

function decodeFriendlyTimeString(timeStr) {
    // Decode strings of the format:
    //   1234
    //   1.45
    //   4567s
    //   1m2s
    //   2h1m40s
    //   2m
    //   2m1.5s
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
    if (document.getElementById("menu").style.display = document.getElementById("menu").style.display == "block") {
        // Menu already open - close it
        document.getElementById("menu").style.display = "none";
    }
    else { 
        // Menu not already open - open it

        // Pause video once menu opens, as on mobile the menu covers the video so you wouldn't want it to keep playing
        if (player) {
            player.pauseVideo();
        }

        // Update the list of saved positions each time the menu is opened, so that it is up-to-date (otherwise would need to refresh the page)
        fetchSavedPositions();

        document.getElementById("menu").style.display = "block";
    }

    e.stopPropagation(); // Otherwise it goes through to the onOverlayClick/onOverlayControlsClick and resets the timer!
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
}

function onPlayerReady() {
    console.log("onPlayerReady");

    // If autoplay doesn't work (e.g. on mobile), indicate to user that they have to click play (otherwise will keep saying "loading video...")
    document.getElementById("loading-status").innerText = 'Ready to play';
    document.title = getSafeTitle();

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
    // Clamp to valid values, to prevent weird display of seek times (negative, or never gets to the target time)
    if (target < 0) {
        target = 0;
    }
    if (player.getDuration() && target > player.getDuration()) { // getDuration may return 0 if not available, according to docs
        target = player.getDuration();
    }

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

// Gets the video title, filtering out potential spoilers
function getSafeTitle() {
    var title = player.getVideoData().title;
    // The video title may have something like "X vs Y Game 5", which tells you it goes to game 5, so hide this
       var r = /Game \d+/ig;
    title = title.replace(r, "Game _");
    return title;
}

function onPlayerStateChange(event) {
    console.log("onPlayerStateChange: " + event.data);
    // Toggle visibility of blocker box to hide related videos bar at bottom, which can spoil future games.
    // Also hide the video title, as it may have something like "X vs Y Game 5", which tells you it goes to game 5
    if (event.data == YT.PlayerState.PAUSED) {
        document.getElementById('blocker-bottom').style.display = 'block';
        document.getElementById('blocker-top').style.display = 'block';
        document.getElementById('play-pause-button').style.backgroundImage = "url('static/play.png')";
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

        document.getElementById('play-pause-button').style.backgroundImage = "url('static/pause.png')";
        // Hide after a short delay, as it takes a short time for the related videos bar to disappear
        window.setTimeout(function () {
            // Make sure video hasn't been paused again during the timer
            if (player.getPlayerState() == YT.PlayerState.PLAYING) {
                document.getElementById('blocker-bottom').style.display = 'none';
                document.getElementById('blocker-top').style.display = 'none';
                document.getElementById('blocker-full').style.display = 'none';
            }
        }, 250);
    }
    else if (event.data == YT.PlayerState.ENDED) {
        // Hide related videos that fill the player area at the end of the video
        document.getElementById('blocker-full').style.display = 'block';
        document.getElementById('play-pause-button').style.backgroundImage = "url('static/play.png')";
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
        // Don't hide the controls if the menu is open
        if (document.getElementById("menu").style.display == "none") {
            document.getElementById("player-overlay-controls").style.display = 'none';
        }
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
        // Also close the menu if it's open, so it isn't open the next time the user brings up the controls
        document.getElementById("menu").style.display = 'none';
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
    var effectiveCurrentTime = getEffectiveCurrentTime();

    document.getElementById("current-time-span").innerText = toFriendlyTimeStringColons(effectiveCurrentTime);
    document.getElementById("current-time-span").style.backgroundColor = isSeeking() ? 'orange' : 'white';

    if (player && player.getPlayerState() != YT.PlayerState.UNSTARTED)  // Video may not yet have been loaded
    {
        // Update URL to reflect the current time in the video, so refreshing the page (or closing and re-opening
        // the browser will resume the video at the current time).
        // This doesn't behave quite like we want with the Chrome global history though (it has one entry per timestamp!).
        // See this firefox bug report for some discussion: https://bugzilla.mozilla.org/show_bug.cgi?id=753264
        var params = new URLSearchParams(window.location.search);
        params.set('videoId', player.getVideoData().video_id);
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
                    'video_id': player.getVideoData().video_id,
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
        // There seem to be issues with requesting a small start time != 0, especially for live streams. So put a lower limit on it.
        if (startTime < 10) {
             startTime = 10;
        }
        startTime = Math.round(startTime); // Fractional numbers are invalid and won't work!

        seekTarget = startTime; // Treat the start time as a seek target, so the UI shows this time rather than 0 when loading

        player = new YT.Player('player', {
            height: '100%',
            width: '100%',
            videoId: params.get('videoId'),
            playerVars: {
                'controls': 0,
                // Note that if 'start' is not set (or set to something invalid), then YouTube has some kind of memory where it tries
                // to start where you last were. We don't want this, as we handle it ourselves, so make sure to always set this, even if it's zero (see above)
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

updateSignedInStatus();

// If the user is already signed in, update the list of saved positions
// Even though we update this when the user opens the menu, we also do it now because otherwise the size of the <select>
// dropdown doesn't adjust properly once it's already open (you get a scrollbar which isn't so nice)
fetchSavedPositions();
