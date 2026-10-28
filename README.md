# GigaMux
Universal, instant, Visually Lossless bulk re-encoder made in Rust, with ffmpeg as the backend. Built for reducing the file size of OBS recordings and Twitch vods, without losing any perceptible data.

## Usage

### Overview & Use Case
The intention is to trim unnecessary data from video files that were made heavy on purpose; live encodes generally target a Constant Bitrate (CBR) with a fast preset in order to record as much detail within a specified bitrate/overhead budget. For example:
![OBS CBR ~40 Mbps, P1 Fastest](docs/images/obs-encoder-cbr-40mbps.png)
This OBS encoder setup sacrifices disk space for quality (40mbps) without overwhelming the encoder (P1: Fastest). There are many great applications for this style of encode setting, but this will obviously take up an unnecessary amount of space. Twitch VODs have a lower bitrate as per their [Broadcasting Guidelines](https://help.twitch.tv/s/article/broadcasting-guidelines?language=en_US), but there's still likely to be a large amount of excess data in them, because encoding visually lossless VBR requires more resources, and while streaming protocols support it, VBR is discouraged or rejected on streaming *platforms* for QoS reasons.

Thus, GigaMux will re-encode your input file with `CQ=0`/`CRF=0` (same target, different name and logic depending on the output codec) with the maximum bitrate set from the average rate of the original file (though this can be overridden, if preferred). This way, the visual quality is completely preserved (to the best of the destination codec's ability).

This is a **LOSSY **encode target. For most applications (Downloaded Twitch VODs & local OBS recordings with CBR) there will be no perceptible difference in quality, but the output is still removing data that was included in the input. The question for the user is whether that input matters to them more than storage. For most Twitch streamers or archivists, storage is a constraint that matters more, due to the astronomical size that VODs can reach.

Audio is copied directly from the source to the destination, unless the destination container is incompatible, in which case the audio will be recoded to a compatible codec. The UI will warn if this will occur on render.
