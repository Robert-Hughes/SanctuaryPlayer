Run the following PowerShell commands to package all the source files into a single HTML page:

cd D:\Programming\Media\YouTubeNoSpoilers
Get-Content index.html | E:\Programming\Python\python -m htmlark --ignore-errors | Set-Content single-page/YouTubeNoSpoilers.html

Copy to dropbox:

cp single-page/YouTubeNoSpoilers.html D:\Dropbox\Utilities\YouTubeNoSpoilers\


TODO:

* Prevent arrow keys from changing play speed when it's focused (as they also control the video)
* Sync somehow between PC and phone
* Seems to be a delay sometimes when clicking to toggle the overlay - maybe fixed now, probably a Chrome bug?
* Mobile autoplay doesn't work, so maybe indicate to user that htey have to click play? Otherwise just says "loading video..."
* Sometimes video doesn't load correctly - possibly due to ordering of scripts, what if the youtube api loads before our script.js has been loaded?
    * Could this be because we hav a timer which overwrites the location periodically, so the new proper location gets overwritten before the browser actually
    navigates? Maybe this is only an issue when using the Change Video option, rather than refreshing the page?
* When screen is narrow (e.g. phone portrait), the UI at the bottom wraps poorly
* Disable zooming? I think double-tapping seems to zoom the view so when you exit fullscreen it's zoomed in