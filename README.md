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
* When viewing live video but behind live, turning the screen off (mobile) and on resets to live!
* Better time format support, mainly for the display at the bottom and the "Change Time" dialog
* Change Video sometimes doesn't work, need to keep retrying - could it be the background timer overwriting location?
* The 2s offset for live videos doesn't always work - maybe we need to increase this or do something cleverer?
* See if we can enable Samsung video enhancements (it seems to only work for certain apps, not Chrome unfortunately)
* Disable selection, to prevent double clicking a lot from sometimes making the whole screen blue
* Loading saved positions seems to not get the time right, it's often behind by a few minutes? Maybe only on longer videos (percentage error?)
* on mobile, refreshing the menu on open causes a flicker when you open the menu
* The changeVideo code is very similar to the saved position stuff - could/should it be shared/forwarding to each other?
