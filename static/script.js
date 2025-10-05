var player;
var doSomethingAfterDelayTimerIds = new Map();
var timerId;
 // During seek operations, we store the target time so that repeated seeks can offset relative to this rather than the current
 // video time, which can lag behind. This is reset to null once the seek has completed.
var seekTarget = null;
var lastUploadedPosition = null; // The last video position successfully uploaded to the sever (for syncing time across devices)
// The wall clock time when the video first started playing. Used to hide spoilers (see usages).
var firstPlayTime = null;

// Remembers the current state of the top and bottom 'blocker' bars, which are implemented
// using clip-path on the player. Although we could instead extract this information from the style.clipPath
// string, that would be a bit clunky.
let topBlockerShowing = false;
let bottomBlockerShowing = false;

var videoPlatform = null; // 'twitch' or 'youtube'
var isTwitch = false;
var isYoutube = false;

const youtubeVideoIdRegex = /^[A-Za-z0-9-_]{11}$/; // 11 alphanumeric chars (plus some special chars)
const twitchVideoIdRegex = /^\d{10}$/; // 10-digit number

let videoId; // The ID of the video we are playing (extracted from URL on page load)

// Title and release date of the current video, which we can't get from the player object so it comes from our server.
// This won't be available immediately as we query it from our own server.
var metadataFromServer;

// Once the user changes the quality themselves, we disable the automatic selection of quality
// otherwise we might immediately change it back to the user's favourite quality.
var hasManuallyChangedQuality = false;

// Decodes a 'human-friendly' time string (like '1h30') into a number of seconds
function decodeFriendlyTimeString(timeStr) {
    // Decode strings of the format:
    //   1234 (1234 seconds)
    //   1.45 (1 minute 45 seconds)
    //   4567s (4567 seconds)
    //   1m2s (1 minute 2 seconds)
    //   2h1m40s (2 hours 1 minute 40 seconds)
    //   2m (2 minutes)
    //   8h20 (8 hours 20 minutes)
    //   2:34:57 (2 hours 34 minutes 57 seconds)
    //   3:45 (3 minutes 45 seconds)
    //   :40 (40 seconds)
    //   1: (1 minute)

    // Split up the digit groups, note that the brackets for regex capturing mean that the seperators are also included in the resulting array.
    let parts = timeStr.split(/([^0-9]+)/);
    // We should now have an array that alternates between a number and a seperator, e.g. ["1", "h", "30", "m"]
    // Note that the way that split() works it will always return an odd number of elements, but the first/last elements may be empty strings if
    // the string started/ended with a non-digit character.

    // If we don't encounter a separator which specifies the units (like h, m or s), then we assume based
    // on the number of parts. e.g. for 1:23:45 it starts with hours, but 1234 is just seconds.
    let defaultPlaceValue = parts.length == 1 ? 1 :
                            parts.length == 3 ? 60 : 3600;
    let currentPlaceValue = defaultPlaceValue; // We'll update this as we go

    let result = 0;
    for (var i = 0; i < parts.length; i += 2) {
        let value = parts[i] == '' ? 0 : parseInt(parts[i]);
        // If the seperator specifies h/m/s, then use that as the place value, otherwise assume that it's
        // the value we calculated on the previous iteration (reducing place value)
        let seperator = parts[i + 1];
        currentPlaceValue = seperator == 'h' ? 3600 :
                            seperator == 'm' ? 60 :
                            seperator == 's' ? 1 : currentPlaceValue;

        result += value * currentPlaceValue;

        // Assume that the place value decreases as we go (h -> m -> s)
        currentPlaceValue = currentPlaceValue / 60;
    }

    return result;
}


// e.g. 3601 -> "1h00m01s"
function toFriendlyTimeString(seconds) {
    var hours = Math.floor(seconds / 3600);
    var minutes = Math.floor((seconds % 3600) / 60);
    var seconds = Math.floor(seconds % 60);

    return hours + 'h' + minutes.toString().padStart(2, '0') + 'm' + seconds.toString().padStart(2, '0') + 's';
}

// e.g. 3601 -> "1:00:01"
function toFriendlyTimeStringColons(seconds) {
    var hours = Math.floor(seconds / 3600);
    var minutes = Math.floor((seconds % 3600) / 60);
    var seconds = Math.floor(seconds % 60);

    return hours + ':' + minutes.toString().padStart(2, '0') + ':' + seconds.toString().padStart(2, '0');
}

// Converts an ISO format date/time string like "2021-01-01T12:34:56Z" into a human-friendly string
// describing how long ago that time was, like "2 days ago".
function getRelativeDateTimeString(isoString) {
    var date = new Date(isoString);
    if (isNaN(date.valueOf())) {
        // This indicates an invalid string format
        return "?";
    }
    var now = new Date();

    var diff = now - date;
    var seconds = diff / 1000;
    var minutes = seconds / 60;
    var hours = minutes / 60;
    var days = hours / 24;

    function pluralize(n, x) { return n == 1 ? ("1 " + x) : (n + " " + x + "s"); }

    if (days >= 365) {
        return pluralize(Math.floor(days / 365), "year") + " ago";
    }
    if (days >= 1) {
        return pluralize(Math.floor(days), "day") + " ago";
    }
    if (hours >= 1) {
        return pluralize(Math.floor(hours), "hour") + " ago";
    }
    if (minutes >= 1) {
        return pluralize(Math.floor(minutes), "minute") + " ago";
    }
    return "Just now";
}

// Works out the difference between the two times (in seconds) and expresses it as a human-friendly string,
// e.g. "+2 minutes", "-3 hours"
function getRelativeTimeString(subjectSeconds, relativeToSeconds) {
    var absDiff = Math.abs(subjectSeconds - relativeToSeconds);
    var seconds = absDiff;
    var minutes = seconds / 60;
    var hours = minutes / 60;

    var sign = subjectSeconds > relativeToSeconds ? "+" : "-";

    function pluralize(n, x) { return n == 1 ? ("1 " + x) : (n + " " + x + "s"); }

    if (hours >= 1) {
        return sign + pluralize(Math.floor(hours), "hour");
    }
    if (minutes >= 1) {
        return sign + pluralize(Math.floor(minutes), "minute");
    }
    if (seconds >= 1) {
        return sign + pluralize(Math.floor(seconds), "second");
    }
    return "Same";
}

// '50px', _ -> 50
// '20%, 50 -> 10
function convertPercentageToPixels(x, size) {
    if (x.endsWith("%")) {
        let percent = parseFloat(x.substring(0, x.length-1));
        return size * percent / 100.0;
    } else if (x.endsWith("px")) {
        return parseFloat(x.substring(0, x.length-2));
    } else {
        return parseFloat(x);
    }
}

function closeMenu() {
    document.getElementById("menu").style.display = "none";
    document.getElementById("menu-button").classList.remove("open");
}

function onMenuButtonClick(e) {
    if (window.getComputedStyle(document.getElementById("menu")).display == "flex") {
        // Menu already open - close it
        closeMenu();
    }
    else {
        // Menu not already open - open it

        // Pause video once menu opens, as on mobile the menu covers the video so you wouldn't want it to keep playing
        if (isVideoLoadedSuccessfully()) {
            pause();
        }

        // Update the list of saved positions each time the menu is opened, so that it is up-to-date (otherwise would need to refresh the page)
        fetchSavedPositions();

        // Update list of available quality levels each time the menu is opened, as these aren't
        // available immediately when the video is first loaded
        document.getElementById("quality-select").options.length = 0; // Clear any old options
        if (isVideoLoadedSuccessfully()) {
            document.getElementById("quality-row").style.display = "block";
            for (var quality of getAvailableQualities()) {
                var opt = document.createElement("option");
                opt.value = quality;
                opt.text = quality;
                document.getElementById("quality-select").options.add(opt);
            }
            document.getElementById("quality-select").value = getCurrentQuality();
        } else {
            document.getElementById("quality-row").style.display = "none";
        }

        document.getElementById("menu").style.display = "flex";
        document.getElementById("menu-button").classList.add("open");
    }
}

function isVideoLoadedSuccessfully() {
    // Both YouTube and Twitch will only report the video ID once it has been loaded
    if (isYoutube && player && player.getVideoData) {
        return !!player.getVideoData().video_id;
    } else if (isTwitch && player && player.getVideo) {
        return !!player.getVideo();
    }
    return false;
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

// Gets an array of strings, with the name of each available quality level
function getAvailableQualities() {
    let result = [];
    if (isYoutube) {
        result = player.getAvailableQualityLevels();
    } else {
        // The Twitch player API returns an array of objects, each with a few properties. Although .name seems
        // like it should be the one to use, it's .group that we need for setQuality and getQuality.
        result = player.getQualities().map(q => q.group);
    }
    // Both APIs have a quirk when the video has recently finished loading - the available quality levels
    // return an empty array, but the current quality can still be queried. To avoid this causing problems
    // for our dropdown, we ensure that there is always an entry for the current quality.
    let current_quality = getCurrentQuality();
    if (!result.includes(current_quality)) {
        result.push(current_quality);
    }

    return result;
}

function getCurrentQuality() {
    if (isYoutube) {
        return player.getPlaybackQuality();
    } else {
        return player.getQuality();
    }
}

function setQuality(quality) {
    if (isYoutube) {
        // Even though the youtube player has an API to set the quality (setPlaybackQuality), it doesn't actually do anything.
        // However the iframe player does have a UI to change the quality, and this is remembered in localstorage
        // so we can instruct the user to do this instead.
        // Unfortunately this isn't possible using the 'use native controls' option as we load the iframe with controls=0
        // which hides the settings button, so we have to open a new iframe (on a known safe video) with controls=1 to let the
        // user change the quality
        // Note: the localstorage option is scoped within our domain, so the user has to set it on an iframe from our site (not youtube.com directly)
        if (confirm("YouTube does not allow changing the video quality programmatically. " +
            "Instead, a new window will open with a placeholder YouTube video where you can change the quality using the settings button (play the video, then click the gear icon). " +
            "After changing the quality, refresh this page to apply the new setting. " +
            "This should be remembered for future YouTube videos on this site. ")) {
            let iframe = document.createElement("iframe");
            iframe.src = "https://www.youtube.com/embed/gWFbES7PvRE?controls=1"
            iframe.style.width = "100%";
            iframe.style.height = "100%";
            // Replace the entire page with this iframe. This is quite hacky (and might have awkward interaction with
            // any timers or async things we have running), but it's easy and the user will refresh the page anyway
            // so shouldn't be a big problem.
            document.body.innerHTML = "";
            document.body.appendChild(iframe);
        } else {
            // revert the <select> value, otherwise it looks like it changed successfully
            document.getElementById("quality-select").value = getCurrentQuality();
        }
    } else {
        player.setQuality(quality);
    }
}

// If the user has set some favourite qualities and one of them is available,
// set it to that.
function setQualityToFavourite() {
    let favouriteQualitiesStr = localStorage.getItem("favourite_qualities") ?? "";
    // Split on commas or semicolons
    let favouriteQualities = favouriteQualitiesStr.split(/[,;]/);
    let availableQualities = getAvailableQualities();
    for (let favouriteQuality of favouriteQualities) {
        if (favouriteQuality != "" && availableQualities.includes(favouriteQuality)) {
            if (favouriteQuality != getCurrentQuality()) {
                setQuality(favouriteQuality);
            }
            return true;
        }
    }
    return false;
}

function onSetFavouriteQualitiesClick(event) {
    var x = prompt("(Twitch only) Enter a comma-separated list of quality names. If one of these is available when playing a video then it will be chosen automatically:", localStorage.getItem("favourite_qualities") ?? "");
    if (x) {
        localStorage.setItem("favourite_qualities", x);
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

        document.getElementById("saved-positions-loading").style.display = "inline-block";
        fetch("get-saved-positions?" + params.toString())
            .then(response => {
                if (!response.ok) {
                    throw new Error('get-saved-positions response was not OK');
                }
                return response.json();
            })
            .then(response => {
                // Clear previous entries
                document.getElementById("saved-positions-table").tBodies[0].innerHTML = "";

                // Add new ones
                // Find the furthest position for the current video so that we can highlight this row,
                // as it's likely the one the user wants to choose (i.e. you've been watching this video
                // on another device and want to continue watching it here). It might end up being the most
                // recent saved position and so at the top of the list anyway, but not if this device has
                // the furthest position, in which case we wouldn't want to highlight an older time.
                var maxPosition = null;
                var highlightIdx = null;
                for (var i = 0; i < response.length; ++i) {
                    var savedPosition = response[i];
                    if (savedPosition.video_id === videoId) {
                        if (maxPosition == null || savedPosition.position > maxPosition) {
                            maxPosition = savedPosition.position;
                            highlightIdx = i;
                        }
                    }
                }

                for (var i = 0; i < response.length; ++i) {
                    var savedPosition = response[i];
                    // Skip the row if it's the same video, device and position as currently playing.
                    // (Checking just device and video is probably sufficient in nearly all cases, but there might be some corner
                    //  cases e.g. where the position hasn't been uploaded)
                    if (savedPosition.video_id === videoId && savedPosition.device_id == localStorage.getItem("device_id") &&
                        Math.abs(savedPosition.position - getEffectiveCurrentTime()) < 10) {
                            continue;
                        }
                    var r = createSavedPositionTableRow(savedPosition, i == highlightIdx);
                    document.getElementById("saved-positions-table").tBodies[0].appendChild(r);
                }

                // Show relative date times immediately, rather than waiting for the next timer
                updateRelativeDateTimeElements();
            })
            .catch((error) => {
                console.error('Error getting positions from server:', error);
            })
            .finally(() => {
                document.getElementById("saved-positions-loading").style.display = "none";
            });
    }
}

function createSavedPositionTableRow(savedPosition, isHighlight) {
    var row = document.createElement("tr");
    row.classList.add("clickable");
    if (isHighlight) {
        row.classList.add("highlight");
    }

    var linkParams = new URLSearchParams(window.location.search);
    linkParams.set('videoId', savedPosition.video_id);
    linkParams.set('time', savedPosition.position);

    // Each cell in the row has an <a> tag in it so that the browser treats it as a regular link,
    // so that it can be (e.g.) ctrl-clicked to open the video in a new tab.
    // However we want to intercept the regular click case so that we can avoid reloading the page
    // if all we want to do is seek within the current video.

    function onCellLinkClick(event) {
        // If the user is using a modifier (Ctrl, Cmd, Shift, Middle-click) then let the browser handle it normally
        // so that the video can be opened in a new tab/window etc.
        // Also if the link is for a different video then we'll need to reload the page anyway, so let the browser handle it.
        if (savedPosition.video_id !== videoId || event.ctrlKey || event.metaKey || event.shiftKey || event.altKey || event.button !== 0){
            return true;
        }
        // But if it's a normal left-click and it's for the same video then we can just do a seek
        seekTo(savedPosition.position);
        closeMenu();

        event.preventDefault();
        return false;
    }

    function addCell(cell) {
        var cell = document.createElement("td");
        let a = document.createElement("a");
        a.href = "?" + linkParams.toString();
        a.addEventListener('click', onCellLinkClick);
        cell.appendChild(a);
        row.appendChild(cell);
        return a;
    }

    var cell = addCell();
    // Rather than converting to a string now, we store the absolute date/time and mark it so that the display will be updated dynamically
    cell.classList.add("relative-date-time");
    cell.dataset["isoDatetime"] = savedPosition.modified_time;

    var cell = addCell();
    cell.textContent = savedPosition.device_id;

    var cell = addCell();
    cell.textContent = toFriendlyTimeStringColons(savedPosition.position);
    if (savedPosition.video_id === videoId) {
        cell.textContent += " (" + getRelativeTimeString(savedPosition.position, getEffectiveCurrentTime()) + ")";
    }

    var cell = addCell();
    if (savedPosition.video_title) {
        cell.textContent = stripSpoilersFromTitle(savedPosition.video_title);
    } else {
        cell.textContent = savedPosition.video_id; // Fallback to video ID if title is missing
    }

    var cell = addCell();
    // Rather than converting to a string now, we store the absolute date/time and mark it so that the display will be updated dynamically
    cell.classList.add("relative-date-time");
    cell.dataset["isoDatetime"] = savedPosition.video_release_date;

    return row;
}


function changeVideo() {
    var userValue = prompt("Please enter YouTube/Twitch video URL or video ID:");
    if (!userValue) {
        return;
    }

    // Extract video ID and time from URL/string. Needs to work for:
    //   3fgD9k8Hkbc (YouTube video ID)
    //   2395077199 (Twitch video ID)
    //   https://youtu.be/3fgD9k8Hkbc
    //   https://youtu.be/3fgD9k8Hkbc?t=3839
    //   https://www.youtube.com/watch?v=3fgD9k8Hkbc
    //   https://www.youtube.com/watch?v=3fgD9k8Hkbc&t=54m39s
    //   https://www.youtube.com/watch?v=3fgD9k8Hkbc&t=54m39s&bob=someting
    //   https://youtube.com/watch?v=jDqFz0ZzoZo&si=3b7duJGqF-SKQqKI
    //   https://www.twitch.tv/videos/2386400830
    //   https://www.twitch.tv/videos/2386400830?t=1h29m24s
    var videoId = null;
    var time = null;

    // First try parsing it as a URL
    url = URL.parse(userValue);
    if (url) {
        if (url.hostname.includes("youtube") || url.hostname.includes("youtu.be")) {
            // The video ID could be the last part of the path, or it could be a query string paramater
            videoId = url.searchParams.get("v");
            if (videoId == null) {
                var lastPart = url.pathname.split("/").pop();
                var match = lastPart.match(youtubeVideoIdRegex);
                if (match) {
                    videoId = match[0];
                }
            }
        } else if (url.hostname.includes("twitch")) {
            // The video ID is the last part of the path
            var lastPart = url.pathname.split("/").pop();
            var match = lastPart.match(twitchVideoIdRegex);
            if (match) {
                videoId = match[0];
            }
        }

        // time is always a query string parameter (same for YouTube and Twitch)
        time = url.searchParams.get("t");
    } else {
        // Fallback to a simpler regex search, just to get the video ID (no time)
        var match = userValue.match(youtubeVideoIdRegex);
        if (match) {
            videoId = match[0];
        }
        match = userValue.match(twitchVideoIdRegex);
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

function updateVideoTitle() {
    let title = getSafeTitle();
    if (title) {
        document.title = title;
        // The blocker box at the top hides the video title, as it may contain spoilers, so we hide it, but show a filtered version of the title instead.
        document.getElementById("video-title").innerText = title;
        // Rather than converting the release date to a string now, we store the absolute date/time and it will be updated dynamically (because of the element's class name)
        document.getElementById("video-release-date").dataset["isoDatetime"] = metadataFromServer?.video_release_date;
        // Show the relative date time immediately, rather than waiting for the next timer
        updateRelativeDateTimeElements();
    }
}

function onPlayerReady() {
    console.log("onPlayerReady");

    // Even though this is the 'on ready' callback, there might have been an error loading the video, for example
    // if the videoId was invalid. Check if the video metadata loaded properly, which seems to be a good indication for a
    // successful load
    if (isVideoLoadedSuccessfully()) {
        document.getElementById("loading-status-container").style.display = "none";
        // Now that the player is loaded, we can get and show the title (for YouTube at least, for Twitch we have to wait for our server)
        // so this will show a placeholder for now.
        updateVideoTitle();

        // Show the video controls, now that they will work
        document.getElementById("player-overlay-controls-mid").style.display = 'flex';
        document.getElementById("player-overlay-controls-bot").style.display = 'flex';

        // Initialize the speed drop-down with the available playback rates
        for (var rate of getAvailablePlaybackRates()) {
            var opt = document.createElement("option");
            opt.value = rate;
            opt.text = rate + "x";
            document.getElementById("speed-select").options.add(opt);
        }
        document.getElementById("speed-select").value = getCurrentPlaybackRate();

        if (isTwitch) {
            // Twitch remembers your last volume settings, so we reset these each time otherwise
            // the video might be muted (or low volume) and you would have no way to fix it
            // Note that YouTube seems to always reset to 100%, so no need to do this for YouTube.
            player.setMuted(false);
            player.setVolume(1.0);

            // The Twitch player displays a static thumbnail over the top of the video whilst it is loading,
            // but it never removes this once the video is loaded (until you play the video). It also seems
            // to have a weird bug where the thumbnail is the wrong size and offset, so you can see the video
            // behind it too. Both of these are annoying (and the thumbnail could contain spoilers), so we try
            // our best to hide it.
            // The only way I've found so far is to call this no-op API once the video
            // is loaded, which hides the thumbnail. Unfortunately the thumbnail is still visible for a bit
            // before this callback is run, but it's better than nothing!
            //
            // This also has another unexpected benefit - it seems to remove the video title and progress bar
            // from the player! Which means we can be more relaxed about showing our blockers over these.
            // Note that this does seem to take a short time to take effect - the player will still show the title/progress bar
            // for a few seconds after this call.
            player.setVideo("invalid");
        }

        // Update UI immediately, rather than waiting for the first tick
        onTimer();

        // Start background refresh timer
        // Note we don't start it before now, otherwise the player.getCurrentTime() might return 0 and we don't want
        // to report that.
        timerId = window.setInterval(onTimer, 500);

        // Begin fetching additional metadata about the video from our server.
        // We can't get the Twitch video title from the player and looking it up via a separate request to the Twitch API requires
        // an auth token which can't be sent from the client side (otherwise it would leak our token!).
        // We could scrape it from the video's web page, but the browser will block that due to CORS.
        // Instead we fetch it from our server which can do the query for us (without CORS) - yuck!
        // The initial title is set to a placeholder and will be updated when the server responds.
        fetch("get-video-metadata?video_id=" + videoId + "&video_platform=" + videoPlatform)
            .then(response => {
                if (!response.ok) {
                    throw new Error('get-video-metadata response was not OK');
                }
                return response.json()
            })
            .then(response => {
                metadataFromServer = response;
                updateVideoTitle(); // Display the new data
            })
            .catch((error) => {
                console.error('Error:', error);
            })
            .finally(() => {
            });


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

    // Seeking the video makes the bar appear at the bottom
    // and the video title at the top, so we have to show the blockers when seeking,
    // (Twitch) Note this is only the case for shorter seeks (e.g. 5 secs), as for longer seeks we experience a brief 'pause' then 'unpause' which
    // shows the blockers anyway.
    updateBlockerVisibility(true, true);
    hideBlockersShortly();

    onTimer(); // Update UI to show that we are seeking (as the player state change callback might be delayed)
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
        title = metadataFromServer?.video_title; // We can't get this from the Twitch player object, and instead query it from our server and store it here
    }

    if (!title) {
        return null; // Title may not be available yet, e.g. hasn't been loaded from our server yet
    }

    return stripSpoilersFromTitle(title);
}

function stripSpoilersFromTitle(title) {
    // The video title may have something like "X vs Y Game 5", which tells you it goes to game 5, so hide this
    var r = /Game \d+/ig;
    title = title.replace(r, "Game _");

    // Team names in title (e.g. EG vs LLL) can ruin the teams playing in the last game of the day which could be a spoiler
    // if it's tiebreaker that may or may not happen depending on other games. (if the title is updated throughout live video's
    // original broadcast, then the title we see afterwards is the final title).
    // Examples:
    //  GAM vs. DRX | Worlds Game 3
    //  GAM vs. DRX
    //  Team 1 vs. Team 2
    //  Team OGOBAG vs tramula big face | Champions match
    //  Today | A A vs B B | Now
    //  Nothing to see here!
    //  This is a game between A vs B
    //  BAG v TAG
    //  Not gonna match chavs is it
    //  FIRST STAND DO OR DIE DAY FOR EU - HLE VS CFO // KC VS TES !dpmlol !discord !displate - caedrel on Twitch
    //  FIRST STAND INTERNATIONAL DAY 2 KC VS CFO // TL VS LIQUID #FirstStandWatchParty !dpmlol !discord !displate - caedrel on Twitch
    //  aaaaa vs bbbbbb | cddddd vs dddddd | eeeeee vs ffffff
    //  aaaaa vs bbbbbb cddddd vs dddddd eeeeee vs ffffff

    // There might be multiple occurences of "vs", so find and process each one at a time. Note that when there are multiple 'vs',
    // we might unintentionally consider some of the 'vs' as part of team names so the final result might not be exactly right but should
    // still hide the spoilers.
    // This search regex will also match one character from the word before and after the 'vs', to simplify the next bit. e.g. "Team Bob vs Team Bill" -> "b vs T"
    let regex = /[^\s]\s+(vs?\.?)\s+[^\s]/ig;
    while ((vs_match = regex.exec(title)) !== null) {
        let matched_word = vs_match[1]; // The 'vs' bit, without the spaces or surrounding chars
        let team_a_end_inclusive = vs_match.index; // Index of the last character of the word before the 'vs'
        let team_b_start_inclusive = vs_match.index + vs_match[0].length - 1; // Index of the first character of the word after the 'vs'

        // Helper function to search through the `title` string from the given search_start_idx and going in the given dir (+/-1) until
        // it decides (using some heuristics) that it's found the end of the team name.
        // Returns the index at which it stops, i.e. the index of the final character in the team name (not the one just past that)
        function search_until_end_of_team_name(search_start_idx, dir) {
            let prev_char = null;
            let idx_of_most_recent_non_whitespace = search_start_idx; // Keep track of this so that we don't needlessly include whitespace at the end of the team name
            // Count the number of words we go past - we assume that after a couple of words we will have reached the end of the team name
            let num_words = 0;
            for (let i = search_start_idx; i >= 0 && i < title.length; i += dir) {
                let this_char = title[i];

                if (this_char == ' ' && prev_char != ' ') {
                    num_words++;
                    if (num_words >= 3) {
                        return idx_of_most_recent_non_whitespace;
                    }
                } else if (this_char == '|' || this_char == '/' || this_char == '\\' || this_char == '-') {
                    // Punctuation usually signifies the end of the name (e.g. a separator in the title)
                    return idx_of_most_recent_non_whitespace;
                }

                if (this_char != ' ') {
                    idx_of_most_recent_non_whitespace = i;
                }
            }
            // If gets to the end of the loop then we reached the end of the title
            return dir > 0 ? title.length - 1 : 0;
        }

        let team_b_end_inclusive = search_until_end_of_team_name(team_b_start_inclusive, +1);
        let team_a_start_inclusive = search_until_end_of_team_name(team_a_end_inclusive, -1);

        let old_length = title.length;
        title = title.substring(0, team_a_start_inclusive) + "_ " + matched_word + " _" + title.substring(team_b_end_inclusive+1); // Note +1 as substring() takes an exclusive index, but our function returns an inclusive index

        // Continue searching for any more 'vs' from the index after what we just replaced (adjusted for index in the new string!)
        regex.lastIndex = title.length - (old_length - team_b_end_inclusive);
    }

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
    if (isTwitch) {
        console.log(player.getPlayerState().playback);
    }

    // Toggle visibility of blocker box to hide related videos bar at bottom, which can spoil future games.
    // Also hide the video title, as it may have something like "X vs Y Game 5", which tells you it goes to game 5
    if (newStateStr == "paused") {
        // Even though we show the pause blockers just before pausing the video, we also do it here just in case the video
        // gets paused through other means (not initiated by us)
        updateBlockerVisibility(true, true);
        document.getElementById('play-pause-button').classList.remove("icon-pause");
        document.getElementById('play-pause-button').classList.add("icon-play");
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

        document.getElementById('play-pause-button').classList.remove("icon-play");
        document.getElementById('play-pause-button').classList.add("icon-pause");

        // Hide the blocker boxes after a short delay, as the UI elements that we're covering take a little time to disappear
        hideBlockersShortly();
    }
    else if (newStateStr == "ended") {
        // Hide related videos that fill the player area at the end of the video
        document.getElementById('blocker-full').style.display = 'block';
        document.getElementById('play-pause-button').classList.remove("icon-pause");
        document.getElementById('play-pause-button').classList.add("icon-play");

        if (isTwitch) {
            player.setVideo("0"); // Stop the player from auto-loading the next video (random hack that seems to work quite well)
        }
    }

    // Keep the UI responsive to changes in player state (rather than waiting for the next tick)
    onTimer();
}

function hideBlockersShortly() {
    // Bottom blocker - related videos bar (YouTube) or video length (Twitch)
    var delay = isYoutube ? 250 : isTwitch ? 5500 : 0;  // Twitch takes longer to hide the video length bar
    doSomethingAfterDelay("hideBottomBlocker", delay, function() {
        // Make sure video hasn't been paused again during the timer
        if (isPlaying()) {
            updateBlockerVisibility(null, false);
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
            updateBlockerVisibility(false, null);
       }
    });
}


function onPlaybackQualityChange(event)
{
    console.log("onPlaybackQualityChange: " + event.data);
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
        // Don't hide the controls if the menu is open or if the lock slider is being dragged.
        // Once the menu is closed or the slider is dropped, the controls should be hidden shortly again
        if (window.getComputedStyle(document.getElementById("menu")).display == "none" &&
            document.getElementById("lock-controls-slider").dragStartX == undefined) {
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
        closeMenu();
        event.stopPropagation();  // Stop the click from going up to the player-overlay, which would show the controls again!
    }

    // Clicking anywhere on the controls (e.g. any button) - refresh the timeout to hide the controls
    hideControlsShortly();

    // Prevent bubbling to the onOverlayClick() handler (causes problems with re-showing the controls that were hidden
    // by clicking the play button, when using Twitch).
    event.stopPropagation();
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
        // Twitch uses the IntersectionObserver API to determine if the player element
        // is visible and not obscured by anything and if it is then this blocks playing the video.
        // To work around this, we temporarily hide our entire overlay, play the video, then re-show our
        // overlay. Twitch is happy with this as it only considers the visibility check when starting/resuming playback.
        // Because the IntersectionObserver API doesn't provide immediate updates, we may have to
        // wait a bit before Twitch is happy to play the video, so we keep trying.
        document.getElementById("player-overlay").style.opacity = "0"; // Hide all of our overlay, so that nothing covers the player
        // Hide the controls immediately rather than waiting for the normal timeout,
        // otherwise when the opacity is set back to 1, the controls will reappear for a second then disappear again,
        // which is confusing.
        document.getElementById("player-overlay-controls").style.display = 'none';
        firstPlayAttemptTime = performance.now();
        let tryPlay = function() {
            if (isPlaying()) {
                // Successful! We can show our overlay again
                document.getElementById("player-overlay").style.opacity = "1";
            } else  {
                // Hasn't worked yet, try again and check in a bit to see if it worked.
                if (performance.now() - firstPlayAttemptTime > 5000) {
                    // It could be that the size of the window is too small and Twitch still refuses to play,
                    // or it could be another problem like the video is still buffering.
                    // Either way, give up after a short time and show the overlay again so the user interact again
                    alert("Video is taking a while to play. This could be because the window is too small - try making the window taller.");
                    document.getElementById("player-overlay").style.opacity = "1";
                    return;
                }
                player.play();
                doSomethingAfterDelay("tryPlay", 100, tryPlay);
            }
        };
        tryPlay();
    }
}

function pause() {
    // Before telling the player to pause, show the blockers. This ensures that they are visible before the player shows the title etc.,
    // so we don't catch a frame that might contain a spoiler.
    updateBlockerVisibility(true, true);

    if (isYoutube) {
        player.pauseVideo();
    } else if (isTwitch) {
        player.pause();
    }
}

// Returns ratio of width / height, or null if not available
function getVideoAspectRatio() {
    if (isYoutube) {
        return 16 / 9; // YouTube API doesn't provide any way to get this :( so just assume 16:9
    } else if (isTwitch) {
        if (player.getPlaybackStats) { // Might not be loaded yet
            let s = player.getPlaybackStats().videoResolution; // e.g. "1920x1080"
            if (s && s != "0x0") {
                return parseInt(s.split("x")[0]) / parseInt(s.split("x")[1])
            }
        }
        return null;
    }
}

// Sets the clip-path of the player element to block out the top and/or bottom sections to hide spoilers.
// If either parameter is null, that blocker's visibility will remain as it was (i.e. no change).
function updateBlockerVisibility(showTopBlocker, showBottomBlocker) {
    if (window.getComputedStyle(document.getElementById("player-overlay")).display === "none") {
        // Overlay is hidden, so must be using 'native controls' mode, in which case we don't want to show any blockers even when asked
        return;
    }

    let player = document.getElementById("player");
    if (showTopBlocker === null) {
        showTopBlocker = topBlockerShowing;
    }
    if (showBottomBlocker === null) {
        showBottomBlocker = bottomBlockerShowing;
    }

    // We used to use <div> elements on the player overlay to block things we didn't want to see, but that caused
    // problems with Twitch's use of the IntersectionObserver API as it was detecting that we were drawing stuff
    // on top of the player and this blocks playing the video.
    // To work around this, we use clip-path instead to clip parts of the player which we don't want to see.
    // Apparently this doesn't trigger the same problem (maybe because it's not drawing over it,
    // it's just never drawing those bits at all)
    let topSize;
    let bottomSize;
    if (isYoutube) {
        topSize = "60px"; // Top blocker needs to be taller than the title bar
        bottomSize = "200px"; // Bottom blocker to be taller than the related videos bar (note this doesn't always seem to show, maybe only on mobile?)
    } else if (isTwitch) {
        topSize = "150px"; // Top blocker needs to be taller than the title bar
        bottomSize = "90px"; // Bottom blocker needs to be taller than the video length bar, but not as tall as for YouTube
    }

    player.style.clipPath = `inset(${showTopBlocker ? topSize : "0px"} 0px ${showBottomBlocker ? bottomSize : "0px"} 0px)`; // Top, right, bottom, left
    // Show our video info at the top whenever the top blocker is showing
    document.getElementById("top-info").style.display = showTopBlocker ? "flex" : "none";

    topBlockerShowing = showTopBlocker;
    bottomBlockerShowing = showBottomBlocker;

    // If blockers are showing then shrink the effective width of the player so that the video is squashed into
    // a smaller area in the middle to make sure that the whole video is visible (otherwise the clipped parts of the
    // player might cover important bits of the video).
    if (topBlockerShowing || bottomBlockerShowing) {
        let requiredLetterboxPixels = 0;
        // Convert percentage sizes to pixels (these are based on the border box, which if offsetHeight).
        if (showTopBlocker) {
            requiredLetterboxPixels = Math.max(requiredLetterboxPixels, convertPercentageToPixels(topSize, player.offsetHeight));
        }
        if (showBottomBlocker) {
            requiredLetterboxPixels = Math.max(requiredLetterboxPixels, convertPercentageToPixels(bottomSize, player.offsetHeight));
        }
        let videoAspectRatio = getVideoAspectRatio();
        if (videoAspectRatio) { // Might not be available yet, like on first page load before the video is ready
            let requiredPlayerWidth = (document.getElementById("root").clientHeight - 2 * requiredLetterboxPixels) * videoAspectRatio;
            // Minimum width, otherwise Twitch player will refuse to play (it also has a minimum height of 150, which we don't check yet)
            // Note that the clip-path may still obscure part of the player and this may cause problems with Twitch's IntersectionObserver
            // (it does have some leeway though) but there's not much we can do if the window is so short.
            requiredPlayerWidth = Math.max(requiredPlayerWidth, 300);

            if (requiredPlayerWidth < document.getElementById("root").clientWidth) {
                let totalMargin = document.getElementById("root").clientWidth - requiredPlayerWidth;
                player.style.marginLeft = (totalMargin / 2) + "px";
                player.style.marginRight = (totalMargin / 2) + "px";
            } else {
                player.style.marginLeft = "0px";
                player.style.marginRight = "0px";
            }
        } else {
            player.style.marginLeft = "0px";
            player.style.marginRight = "0px";
        }
    } else {
        player.style.marginLeft = "0px";
        player.style.marginRight = "0px";
    }

    // The above calculations depend on the video resolution and the player size, so if either
    // of these change then the calculations need to be redone (otherwise the clip path or margins might be wrong).
    // The video resolution will change once the video first loads (isn't available on first load
    // and even after the player is 'ready' there is a short delay (at least for Twitch).
    // The player size can change if the user resizes the window or rotates their phone.
    if (topBlockerShowing || bottomBlockerShowing) {
        doSomethingAfterDelay("updateBlockerVisibility", 100, function() { updateBlockerVisibility(null, null); });
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
    if (isSeeking()) {
        document.getElementById("current-time-span").classList.add("seeking");
    } else {
        document.getElementById("current-time-span").classList.remove("seeking");
    }

    // Clear seek time if we were seeking and have now reached the target.
    // We allow a little leeway as there the player might not seek to exactly the time we requested
    // The Twitch player seems to be quite laggy with reporting time updates, so even after the video has visually
    // finished seeking and the player state change callback is fired, the getCurrentTime() still reports the old value,
    // so we can't check this in onPlayerStateChange() like we used to.
    if (isSeeking() && Math.abs(player.getCurrentTime() - seekTarget) < 2.0)
    {
        seekTarget = null;
    }

    // Check if we should change the quality to the user's favourite. We check this in the timer
    // because quality levels aren't available immediately after the player is ready, and also
    // setting the quality immediately when loading the video doesn't seem to work, so we keep trying
    // until it does.
    // We only do this if the user hasn't manually changed the quality, as we don't want to override their choice.
    if (!hasManuallyChangedQuality) {
        setQualityToFavourite();
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
                'video_id': videoId,
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
        // The YouTube API *replaces* the given element with its iframe, so for consistency with Twitch
        // (which makes a child element), we manually create a child div
        let childDiv = document.createElement("div");
        childDiv.id = "player-child";
        document.getElementById("player").appendChild(childDiv);
        player = new YT.Player(childDiv.id, {
            height: '100%',
            width: '100%',
            videoId: params.get('videoId'),
            playerVars: {
                'controls': 0, // Hide as much of the UI as we can (as we'll provide this).
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
        player.addEventListener(Twitch.Player.ERROR, function(x) { console.log("Twitch error: " + x); });
    }

    // Display blockers before the video loads so that when the video loads but hasn't started playing,
    // the title and related videos (YouTube) and video length bar (Twitch) are hidden
    updateBlockerVisibility(true, true);
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

function onQualitySelectChange(event) {
    hasManuallyChangedQuality = true;
    setQuality(this.value);
}

// Sometimes it's useful to be able to interact directly with the native video player (Youtube or Twitch).
// For example on Twich sometimes there's a popup "Audio for portions of this video has been muted" popup which you want to dismiss.
// Also you might want to hide all the blockers to see something on the video when paused, if you're not concerned about spoilers at that time.
function useNativePlayerControlsClick(event) {
    // Hide any blockers that might be showing
    updateBlockerVisibility(false, false);
    // Hide the overlay, which removes all our controls as well as allowing mouse/keyboard events through
    // to the native player underneath
    document.getElementById("player-overlay").style.display = "none";
    // Provide a way for the user to return to the normal controls (as the Menu won't be accessible any more!)
    document.getElementById("restore-normal-controls-button").style.display = "block";

    // Close the menu, so it isn't still open when the user restores the normal controls
    closeMenu();
}

function restoreNormalControlsClick(event) {
    document.getElementById("player-overlay").style.display = "block";
    document.getElementById("restore-normal-controls-button").style.display = "none";
}

function toggleControlLocking(event) {
    let elementsToToggle = [];
    for (let e of document.getElementsByClassName("clickable")) {
        if (e.id != "lock-controls-slider") {
            elementsToToggle.push(e);
        }
    }

    if (document.getElementById("lock-controls-slider").classList.contains("icon-unlocked")) {
        for (let e of elementsToToggle) {
            e.setAttribute("data-state", "disabled");
        }

        document.getElementById("lock-controls-slider").classList.remove("icon-unlocked")
        document.getElementById("lock-controls-slider").classList.add("icon-locked");
    } else {
        for (let e of elementsToToggle) {
            e.setAttribute("data-state", "enabled");
        }

        document.getElementById("lock-controls-slider").classList.remove("icon-locked")
        document.getElementById("lock-controls-slider").classList.add("icon-unlocked");
    }
}

function lockControlsPointerDown(event) {
    //TOOD: visual indication that this thing needs to be dragged!
    let slider = document.getElementById("lock-controls-slider");
    // Begin dragging the slider - capture future pointer events (mainly so that we have a way of remembering
    // that we are dragging, but this might also help catch pointer events if it moves outside of the slider bounds)
    slider.setPointerCapture(event.pointerId);
    // Remember where the pointer is relative to the slider box, so we can calculate the new position when dragged (see lockControlsPointerMove)
    slider.dragStartX = event.offsetX;

    // Cancel any previous transition if the slider was automatically sliding back to the left
    // by fixing the element in place at its half-transitioned position and then remove the transition effect.
    slider.style.left  = window.getComputedStyle(slider).left;
    slider.style.transition = "none";

    // Hint to the user that they should drag this. Will be updated by javascript whilst dragging.
    slider.style.cursor = "grabbing";
}

function lockControlsPointerMove(event) {
    let slider = document.getElementById("lock-controls-slider");
    if (slider.hasPointerCapture(event.pointerId)) {
        const limit = document.getElementById("root").clientWidth * 0.25;
        let newPos = event.clientX - slider.dragStartX;
        if (newPos > limit) {
            newPos = limit;
            // Indicate that has been dragged far enough to have an effect
            slider.classList.add("glow");
        } else {
            slider.classList.remove("glow");
        }
        slider.style.left = newPos + "px";
    }
}

function lockControlsPointerUp(event) {
    let slider = document.getElementById("lock-controls-slider");
    if (slider.hasPointerCapture(event.pointerId)) {
        slider.releasePointerCapture(event.pointerId);
        slider.dragStartX = undefined;

        slider.style.cursor = "grab"; // Back to normal

        // If dragged far enough, lock/unlock the controls
        const limit = document.getElementById("root").clientWidth * 0.25;
        if (parseFloat(window.getComputedStyle(slider).left) >= limit) {
            closeMenu(); // In case it's open
            toggleControlLocking();
        }

        // Make the slider automatically slide back to the left, over a short period
        slider.style.transition = "left 0.5s";
        slider.style.left = "0px";
        slider.classList.remove("glow");
    }
}

// Goes through every element with the class "relative-date-time" and updates the text to show the
// date/time that the element has been tagged with (via a data- attribute) relative to now (e.g. "1 hour ago").
// This is useful as you may come back to the webpage later and phrases like "1 hour ago" will be wrong.
function updateRelativeDateTimeElements() {
    for (let element of document.getElementsByClassName("relative-date-time")) {
        if ("isoDatetime" in element.dataset) {
            let isoDatetime = element.dataset["isoDatetime"];
            let string = getRelativeDateTimeString(isoDatetime);
            element.textContent = string;
        }
    }
}

function startup() {
    // Hookup event listeners
    document.getElementById("menu-button").addEventListener("click", onMenuButtonClick);
    document.getElementById("change-video-button").addEventListener("click", changeVideo);
    document.getElementById("set-favourite-qualities-button").addEventListener("click", onSetFavouriteQualitiesClick);
    document.getElementById("use-native-player-controls-button").addEventListener("click", useNativePlayerControlsClick);
    document.getElementById("restore-normal-controls-button").addEventListener("click", restoreNormalControlsClick);
    document.getElementById("sign-in-button").addEventListener("click", signIn);
    document.getElementById("sign-out-button").addEventListener("click", signOut);
    document.getElementById("current-time-span").addEventListener("click", changeTime);
    document.getElementById("player-overlay").addEventListener("click", onOverlayClick);
    document.getElementById("player-overlay-controls").addEventListener("click", onOverlayControlsClick);
    document.getElementById("play-pause-button").addEventListener("click", togglePlayPause);
    document.getElementById("fullscreen-button").addEventListener("click", toggleFullscreen);
    document.getElementById("lock-controls-slider").addEventListener("pointerdown", lockControlsPointerDown);
    document.getElementById("lock-controls-slider").addEventListener("pointermove", lockControlsPointerMove);
    document.getElementById("lock-controls-slider").addEventListener("pointerup", lockControlsPointerUp);
    document.getElementById("speed-select").addEventListener("change", onSpeedSelectChange);
    document.getElementById("quality-select").addEventListener("change", onQualitySelectChange);
    document.addEventListener("keydown", onKeyDown)
    for (let button of document.getElementsByClassName("seek-button")) {
        button.addEventListener("click", seekButtonClicked);
    }

    // Start regular timer to update 'relative time' elements
    window.setInterval(updateRelativeDateTimeElements, 60*1000); // Every minute

    var params = new URLSearchParams(window.location.search);
    if (params.has('videoId')) {
        videoId = params.get('videoId');

        document.getElementById("welcome-screen").style.display = "none";
        document.getElementById("loading-status-container").style.display = "flex";
        document.getElementById("use-native-player-controls-button").style.display = 'block';
        document.getElementById("lock-controls-slider").style.display = 'block';

        // Detect if the video ID is for Twitch or YouTube
        if (videoId.match(youtubeVideoIdRegex)) {
            videoPlatform = 'youtube';
            isYoutube = true;

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
        } else if (videoId.match(twitchVideoIdRegex)) {
            videoPlatform = 'twitch';
            isTwitch = true;

            var script = document.createElement('script');
            script.src = "https://player.twitch.tv/js/embed/v1.js";
            script.onload = onAPIReady;
            document.body.appendChild(script)
        }

        if (videoPlatform) {
            document.getElementById("loading-status").innerText = `Loading ${videoPlatform} player...`;
            // When using clip-path for the blockers we'll see the page background so make this black to better match the video player's natural look.
            document.body.style.backgroundColor = "black";
        }
        else {
            document.getElementById("loading-status").innerText = `Error! Unknown video platform for video ID: ${videoId}`;
        }
    }
    updateSignedInStatus();

    // If the user is already signed in, update the list of saved positions
    // Even though we update this when the user opens the menu, we also do it now because otherwise the size of the <select>
    // dropdown doesn't adjust properly once it's already open (you get a scrollbar which isn't so nice)
    fetchSavedPositions();
}

startup();