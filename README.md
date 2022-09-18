Alternative YouTube player which omits various elements which could provide spoilers to esports tournaments.
For example it doesn't show the length of the video being watched and it doesn't show any related videos.

It consists of a single static HTML page along with javascript and css, and so can be run locally from the filesystem,
from a simple web server, or from GitHub's hosting feature if the code is pushed there: https://robert-hughes.github.io/YouTubeNoSpoilers/

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
E:\Programming\google-cloud-sdk\bin\gcloud.ps1 --project youtubenospoilers run deploy youtubenospoilers --source . --region europe-west1 --allow-unauthenticated

Note we are using europe-west1 (Belgium) as it supports simpler custom domains, and is lower CO2 than the London region.

TODO:

* Ability to delete/clean up old saved position data. Perhaps expires after some time?
   - For example if signed in on a device that only used once, it will now always be there!
* Migrate to Google Cloud Run for lower costs? See https://dev.to/pcraig3/cloud-run-vs-app-engine-a-head-to-head-comparison-using-facts-and-science-1225
 Might want to wait ~ a month to see the billing breakdown from YouTubeLiveMonitor first, to make sure it's all as we expect.
* Once Cloud Run is working fine, delete/disable the app engine version? Make sure that the database is kept though! (It seems to be 'part of' the app engine stuff)
* Show if saved position for this video is ahead or behind (e.g. + 2 mins, or 2 mins ahead, 10 mins behind etc.)
* Use subdomain of robdh.uk
* Enable HTTPS?
* If pause the video, after a few seconds, upload the saved position. Otherwise it might be a few seconds behind and never uploaded.
* Consider hiding video title (both when pause video in top left, and also in the Saved Position list), for the case where the video title
 has a spoiler, e.g. "X vs Y Game 5" tells you it goes to game 5...
* Try using Cloud Firestore _client_ libraries in the javascript to directly access the database, rather than having to go via the web server.
THis means the server doesn't need to keep handling requests while watching a video, so would drastically reduce our google cloud usage
* If we do this, then our server doesn't need to be "smart" at all, so could just use a static serving thing (nginx?) rather than a WSGI python thing?
* Buttons to hide the blocker panel, so can see stuff behind it if the user is willing to risk it
* Better button sizing for very small windows, like pop-up window on phone
* Slow-mo/frame-by-frame controls