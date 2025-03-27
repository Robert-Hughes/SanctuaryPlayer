https://sanctuaryplayer.robdh.uk/

Website which plays YouTube and Twitch videos but hides UI elements which could provide spoilers to esports tournaments.
For example it doesn't show the length of the video being watched and it doesn't show any related videos.

It consists of a single HTML page along with javascript and css, and so can be run locally from the filesystem or
from a simple web server.

The index.html is in the root so that when served from GitHub Pages, the URL is shorter (can be accessed with just a /).
The other static files (.js, .css, images) are in a 'static' subfolder, so that it can be
easily served as static content using Flask. Even though serving static files from Flask isn't recommended, the recommended
approach seems to be get a higher level web server (e.g. nginx) to do that, which might be overly
complicated for our simple app.
https://flask.palletsprojects.com/en/2.1.x/quickstart/#static-files

There is an optional web server component which can save the user's progress through videos, so that this can be
synced between different devices. Currently this is set up to use Google Cloud Run. We used to use Google App Engine,
but migrated as Cloud Run seems to result in lower usage of resources - see
https://dev.to/pcraig3/cloud-run-vs-app-engine-a-head-to-head-comparison-using-facts-and-science-1225.

Google Cloud Run deploy command:

(Run from this folder)
E:\Programming\google-cloud-sdk\bin\gcloud.ps1 --project sanctuary-player run deploy sanctuary-player --source . --region europe-west1 --allow-unauthenticated

This is also configured to run automatically when committing to the main branch on GitHub, using GitHub Actions.

Note we are using europe-west1 (Belgium) as it supports simpler custom domains, and is lower CO2 than the London region.

Known Issues
============

* Twitch: sometimes the Twitch player shows a popup about copyright muted audio, which you can't dismiss because we intercept all input. The workaround for this is to click "Use native player controls", then dismiss the popup and then return to the normal controls.
* Twitch - on first load it shows a weird quarter-size frame in the corner before playing the video

TODO:
=====

* Show if saved position for this video is ahead or behind (e.g. + 2 mins, or 2 mins ahead, 10 mins behind etc.)
* If pause the video, after a few seconds, upload the saved position. Otherwise it might be a few seconds behind and never uploaded.
* Slow-mo/frame-by-frame controls
* When video "reloads" while playing (e.g. due to lost connection?), the title appears briefly (not hidden by our blocker box)
* youtube - loading video with a time set and then pressing play results in the time at the bottom briefly jumping to 0 before jumping to the correct time
* Twitch better handling of ENDED - if ends naturally then the blocker doesn't always appear
// also, if refresh the video when it's at the end, the blocker doesn't appear either!
// also seeking away from the end of the video seems a bit broken
* Youtube also has some dodgy behaviour with ended videos and seeking/pressing play
* Twitch - when seeking, the player shows a brief pause then unpause. Seems the docs are wrong about seeking/buffering being counted as playing? OInly for longer seeks?
//  This isn't a big issue, but if the network is slow then it looks like the video is paused when it's just buffering. Maybe we can improve the UI here?
* Twitch - doesn't seem to wake lock the screen, so it turns off after a delay
* http://127.0.0.1:5500/index.html?videoId=2392357391 -> innfinite loading (the Video ID is invalid) -  The ERROR callback thing doesn't seem to work!
* On Android and Twitch video - the blockers cover most of the video! Can we make these smaller?
* For Twitch vod, if phone has been sleeping for a while (with internet off?) and then resume VOD, after a few seconds it shows an error and needs refreshing
* Youtube blocker bar at the top doesn#'t seem to be tall enough to cover the video title
* Improve general display and usability of the UI (icons, the menu, buttons etc.). Make it align with the new icon/branding?
   Hide saved positiosn table when empty?
* Needs to be clearer that the lock button needs to be slid to the right
   * Also, prevent it from being dragged to the left (off the edge of the screen)?
* the saved positions list "2 hours ago" etc. Should update even when not refreshed, so when coming back later it doesn't say "just now"
* Ctrl-click to open saved positions in a new tab. Right click on them too? So maybe just make them an <a>?
* Make saved positions table selectable to copy into clipboard?
* Hover text for relative times (2 days ago) should show the absolute time
* List of saved positions - can we hide the current video on current phone if at current time?
* Move lock slider into player-controls-mid, so moves out of the way when the menu opens
* When first set favourite qualities, it will pretty quickly set the new quality but the dropdown won't update until you close and re-open the menu
* Twitch video start time  - if starting a VOD that you were previously watching live, it seems to ignore this and instead jump to that time?