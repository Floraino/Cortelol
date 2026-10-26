# Bundled FFmpeg for Windows

Cortelol is MIT licensed. The separately bundled `ffmpeg.exe` and `ffprobe.exe` are from the pinned BtbN Windows x64 GPL build recorded in `ffmpeg-win64.json`. This build uses `libx264`, `--enable-gpl`, and `--enable-version3`; its archive includes the GNU GPL version 3 license text. Cortelol invokes these executables as separate processes and does not claim ownership of FFmpeg or its linked libraries.

Before distributing an installer, package the **exact corresponding source** for this static build, including FFmpeg, libx264, every other statically linked library, build scripts and configuration. Publish that source archive alongside the installer on the same download page. The release script refuses to build without the archive and its pinned SHA-256 in `ffmpeg-win64.json`. Review its completeness manually; a matching hash alone cannot establish source compliance.

The installed application includes this notice, Cortelol's MIT license, and the FFmpeg archive's GPL text. The release download must also include the source archive and describe the build provenance. See [FFmpeg's license guidance](https://ffmpeg.org/legal.html), [BtbN build definitions](https://github.com/BtbN/FFmpeg-Builds), and the [pinned binary release](https://github.com/BtbN/FFmpeg-Builds/releases/tag/autobuild-2026-10-25-13-00).
