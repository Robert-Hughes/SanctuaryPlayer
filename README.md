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

TODO:

* Show if saved position for this video is ahead or behind (e.g. + 2 mins, or 2 mins ahead, 10 mins behind etc.)
* If pause the video, after a few seconds, upload the saved position. Otherwise it might be a few seconds behind and never uploaded.
* Try using Cloud Firestore _client_ libraries in the javascript to directly access the database, rather than having to go via the web server.
   THis means the server doesn't need to keep handling requests while watching a video, so would drastically reduce our google cloud usage.
   Not sure how we would handle the google API auth tokens though - don't want these leaked in the client!
   If we do this, then our server doesn't need to be "smart" at all, so could just use a static serving thing (nginx?) rather than a WSGI python thing?
* Slow-mo/frame-by-frame controls
* When video "reloads" while playing (e.g. due to lost connection?), the title appears briefly (not hidden by our blocker box)
* The "loading" spinner on the menu can make the horizontal scroll bar appear
* youtube - loading video with a time set and then pressing play results in the time at the bottom briefly jumping to 0 before jumping to the correct time
* Twitch - on first load it shows a weird quarter-size frame in the corner before playing the video
* improve display of recent videos in menu. Maybe show the video ID too, to distinguish between different videos with the same censored title (_ vs _)
* Twitch better handling of ENDED - if ends naturally then the blocker doesn't always appear
// also, if refresh the video when it's at the end, the blocker doesn't appear either!
// also seeking away from the end of the video seems a bit broken
* Youtube also has some dodgy behaviour with ended videos and seeking/pressing play
* Twitch - when seeking, the player shows a brief pause then unpause. Seems the docs are wrong about seeking/buffering being counted as playing? OInly for longer seeks?
//  This isn't a big issue, but if the network is slow then it looks like the video is paused when it's just buffering. Maybe we can improve the UI here?
* TWitch - set volume to 100% and unmute, as I think it remembers from other vods and then can't be changed! esp. confusing if it's muted at the start!
* Twitch - doesn't seem to wake lock the screen, so it turns off after a delay
* http://127.0.0.1:5500/index.html?videoId=2392357391 -> innfinite loading (the Video ID is invalid) -  The ERROR callback thing doesn't seem to work!
* On Android and Twitch video - the blockers cover most of the video! Can we make these smaller?
* For Twitch vod, if phone has been sleeping for a while (with internet off?) and then resume VOD, after a few seconds it shows an error and needs refreshing
* Youtube blocker bar at the top doesn#'t seem to be tall enough to cover the video title
* Improve general display and usability of the UI (icons, the menu, buttons etc.). Make it align with the new icon/branding?
   Hide saved positiosn table when empty?
* When menu is too large for screen (e.g. Android), oh dear
* On mobile, the menu now opens lower down the screen which means we aren't using the full space available
* Needs to be clearer that the lock button needs to be slid to the right
   * Also, prevent it from being dragged to the left (off the edge of the screen)?
* the saved positions list "2 hours ago" etc. Should update even when not refreshed, so when coming back later it doesn't say "just now"
* Default quality settings?
* Ctrl-click to open saved positions in a new tab. Right click on them too? So maybe just make them an <a>?
* Make saved positions table selectable to copy into clipboard?