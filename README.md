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
synced between different devices. Currently this is set up to use Google App Engine. To upload new code to App Engine:

D:\Programming\Media\YouTubeNoSpoilers> python E:\Programming\google-cloud-sdk\lib\gcloud.py --project youtubenospoilers app deploy

TODO:

* Delete/clean up old saved position data. Perhaps expires after some time?
   - For example if signed in on a device that only used once, it will now always be there!
