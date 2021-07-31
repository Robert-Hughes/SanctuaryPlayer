Run the following PowerShell commands to package all the source files into a single HTML page:

cd D:\Programming\Media\YouTubeNoSpoilers
Get-Content index.html | E:\Programming\Python\python -m htmlark --ignore-errors | Set-Content single-page/YouTubeNoSpoilers.html

Copy to dropbox:

cp single-page/YouTubeNoSpoilers.html D:\Dropbox\Utilities\YouTubeNoSpoilers\

Or can use GitHub's hosting feature as long as this is pushed to GitHub: https://robert-hughes.github.io/YouTubeNoSpoilers/


TODO:

* Sync somehow between PC and phone
* Seems to be a delay sometimes when clicking to toggle the overlay - maybe fixed now, probably a Chrome bug?
* On mobile, the height of the page is wrong when the browser hides the address bar. It also seems there is no way to bring the address bar back!
    - to repro, load page then go into full screen and out again
* When viewing live video but behind live, turning the screen off and on resets to live!
* Support youtube vids embedded on sites like eventvods.com? Or could just use reddit links?
* Better time format support, mainly for the display at the bottom and the "Change Time" dialog
* Seeking while video still loading on first load doesn't update UI properly? (At least if the video has a specified start time). Can limit network on DevTools to help repro.
* If try to seek before 0, display looks weird
* If try to seek after end of video, the orange time stays forever (as video never plays again)