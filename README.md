Run the following PowerShell commands to package all the source files into a single HTML page:

cd D:\Programming\Media\YouTubeNoSpoilers
Get-Content index.html | E:\Programming\Python\python -m htmlark --ignore-errors | Set-Content single-page/YouTubeNoSpoilers.html

Copy to dropbox:

cp single-page/YouTubeNoSpoilers.html D:\Dropbox\Utilities\YouTubeNoSpoilers\


TODO:

* broken unicode on phone
* play speed settings
* quality settings?
* Sync somehow between PC and phone
* Show something when video is loading on first page load
* Indicate when  a start time has been set, before clicking play on the video