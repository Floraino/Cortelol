param(
    [Parameter(Mandatory = $true)][string]$Url,
    [Parameter(Mandatory = $true)][string]$OutputDirectory,
    [switch]$IHaveRightsToMedia
)

# Preserve the old invocation shape, but never launch an unconstrained downloader.
# No URL resolution, process creation, or destination access occurs here.
throw 'Remote video downloads are unavailable until network and process isolation are verified. Download your own uploads through YouTube Studio or Google Takeout, then import the local file. See src-tauri/src/download/YOUTUBE_FEASIBILITY.md.'
