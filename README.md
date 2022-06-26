Alternative YouTube player which omits various elements which could provide spoilers to esports tournaments.
For example it doesn't show the length of the video being watched and it doesn't show any related videos.

It consists of a single static HTML page along with javascript and css, and so can be run locally from the filesystem,
from a simple web server, or from GitHub's hosting feature if the code is pushed there: https://robert-hughes.github.io/YouTubeNoSpoilers/

The index.html is in the root so that when served from GitHub Pages, the URL is shorter (can be accessed with just a /). The other static files (.js, .css, images) are in a 'static' subfolder, to keep separate from the web
server logic which needs to be in the root (so that it works with Google App Engine), and so that it can be 
easily served as static content using Google App Engine without having a complicated filter saying
which files to serve.

I am currently trying to use Google App Engine to host it, which will provide the ability to save and sync the user's position
in videos they are watching, so that it syncs between PC and phone for example. I'm aiming to do this in a way that retains
the ability for static hosting, which might still be convenient.

Google App Engine TODO:

* Consider shortening the URL to not have the static/ prefix? Or redirect "/" to "/static/index" so at least you don't have to type it, but 
 it will still be in the address bar so it's not great
   - ideally it's just /, as it's a single page web-app. The other static content (javascript, images, css etc.) could remain in the static
   folder, as users won't see/use that URL

TODO:

* Sync somehow between PC and phone
  - Could also sync watched videos, so can select recent videos from other devices (use case - just started
  watching a new video on one device, want to continue on another)
   - delete/clean up old data. Perhaps expires after some time?
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
* If try to seek before 0, display looks weird
* If try to seek after end of video, the orange time stays forever (as video never plays again)
* Change Video sometimes doesn't work, need to keep retrying - could it be the background timer overwriting location?
* The 2s offset for live videos doesn't always work - maybe we need to increase this or do something cleverer?
* TUrn off autoplay for desktop, because a) it doesn't work for mobile so it's inconsistent and b) can be annoying especially if tab reloads in the background
* See if we can enable Samsung video enhancements (it seems to only work for certain apps, not Chrome unfortunately)