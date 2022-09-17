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

* Ability to delete/clean up old saved position data. Perhaps expires after some time?
   - For example if signed in on a device that only used once, it will now always be there!
* Migrate to Google Cloud Run for lower costs? See https://dev.to/pcraig3/cloud-run-vs-app-engine-a-head-to-head-comparison-using-facts-and-science-1225
 Might want to wait ~ a month to see the billing breakdown from YouTubeLiveMonitor first, to make sure it's all as we expect.
* Show if saved position for this video is ahead or behind (e.g. + 2 mins, or 2 mins ahead, 10 mins behind etc.)
* Use subdomain of robdh.uk
* Enable HTTPS?
* If pause the video, after a few seconds, upload the saved position. Otherwise it might be a few seconds behind and never uploaded.
* Consider hiding video title (both when pause video in top left, and also in the Saved Position list), for the case where the video title
 has a spoiler, e.g. "X vs Y Game 5" tells you it goes to game 5...
* Try using Cloud Firestore _client_ libraries in the javascript to directly access the database, rather than having to go via the web server.
THis means the server doesn't need to keep handling requests while watching a video, so would drastically reduce our google cloud usage