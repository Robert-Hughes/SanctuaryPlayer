Run the following PowerShell commands to package all the source files into a single HTML page:

cd D:\Programming\Media\YouTubeNoSpoilers
Get-Content index.html | E:\Programming\Python\python -m htmlark --ignore-errors | Set-Content single-page/YouTubeNoSpoilers.html

Copy to dropbox:

cp single-page/YouTubeNoSpoilers.html D:\Dropbox\Utilities\YouTubeNoSpoilers\

Or can use GitHub's hosting feature as long as this is pushed to GitHub: https://robert-hughes.github.io/YouTubeNoSpoilers/


TODO:

* Sync somehow between PC and phone
* Seems to be a delay sometimes when clicking to toggle the overlay - maybe fixed now, probably a Chrome bug?
* Mobile autoplay doesn't work, so maybe indicate to user that htey have to click play? Otherwise just says "loading video..."
* When screen is narrow (e.g. phone portrait), the UI at the bottom wraps poorly
* CHange Video button covers up game time
* More responsive when clicking to skip quickly
* On mobile, sometimes loads the page already zoomed in. Maybe need to set a "viewport" attribute?