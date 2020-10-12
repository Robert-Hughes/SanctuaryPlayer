Run the following PowerShell commands to package all the source files into a single HTML page:

cd D:\Programming\Media\YouTubeNoSpoilers
Get-Content index.html | E:\Programming\Python\python -m htmlark --ignore-errors | Set-Content single-page/YouTubeNoSpoilers.html

Copy to dropbox:

cp single-page/YouTubeNoSpoilers.html D:\Dropbox\Utilities\YouTubeNoSpoilers\


TODO:

* Prevent arrow keys from changing play speed when it's focused (as they also control the video)
* quality settings? - if we can't change them, maybe just show the current quality somewhere
* Sync somehow between PC and phone
* Find better way to fix redraw issue, so it doesn't judder. Maybe replace border within margin or something "equivalent"?
