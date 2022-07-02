Alternative YouTube player which omits various elements which could provide spoilers to esports tournaments.
For example it doesn't show the length of the video being watched and it doesn't show any related videos.

It consists of a single static HTML page along with javascript and css, and so can be run locally from the filesystem,
from a simple web server, or from GitHub's hosting feature if the code is pushed there: https://robert-hughes.github.io/YouTubeNoSpoilers/

The index.html is in the root so that when served from GitHub Pages, the URL is shorter (can be accessed with just a /). 
The other static files (.js, .css, images) are in a 'static' subfolder, to keep separate from the web
server logic which needs to be in the root (so that it works with Google App Engine), and so that it can be 
easily served as static content using Google App Engine without having a complicated filter saying
which files to serve.

There is an optional web server component which can save the user's progress through videos, so that this can be 
synced between different devices. Currently this is set up to use Google App Engine.

TODO:

* delete/clean up old saved position data. Perhaps expires after some time?
* Show video title instead of ID in saved positions
* Seems to be a delay sometimes when clicking to toggle the overlay - maybe fixed now, probably a Chrome bug?
    * This was probably because we were starting loads of timers at the same time :O. THis should be fixed now, but if we still have issues, maybe changing from
      setInterval to setTimeout might prevent multiple timer callbacks from stacking up and 'falling behind'?
* On mobile, the height of the page is wrong when the browser hides the address bar.
    - to repro, load page then go into full screen and out again (possibly twice). 
    - the address bar can be brought back by scrolling the page from the white bit at the bottom
* When viewing live video but behind live, turning the screen off (mobile) and on resets to live!
* Support youtube vids embedded on sites like eventvods.com? Or could just use reddit links?
    - difficult due to cross-site request limitations - would need to access endpoint at https://eventvods.com/api/match/61042f7688832717bf410b2f
    - could be done now with the web server!
    - possibly less important now that we have YouTubeLiveMonitor?
* Better time format support, mainly for the display at the bottom and the "Change Time" dialog
* Seeking while video still loading on first load doesn't update UI properly? (At least if the video has a specified start time). Can limit network on DevTools to help repro.
   - same thing before pressing initial play (if autoplay is disabled)?
* If try to seek before 0, display looks weird
* If try to seek after end of video, the orange time stays forever (as video never plays again)
* Change Video sometimes doesn't work, need to keep retrying - could it be the background timer overwriting location?
* The 2s offset for live videos doesn't always work - maybe we need to increase this or do something cleverer?
* See if we can enable Samsung video enhancements (it seems to only work for certain apps, not Chrome unfortunately)
* Avoid displaying saved positions from this device?
* Disable selection, to prevent double clicking a lot from sometimes making the whole screen blue