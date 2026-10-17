// The page the specs drive: one <video> and one player (hls.js or dash.js), behind
// `window.harness`. Everything a spec needs to observe is a plain value it can poll.
const video = document.getElementById('video');
const errors = [];
let hls = null;
let dash = null;

const ranges = (timeRanges) =>
  Array.from({ length: timeRanges.length }, (_, i) => [timeRanges.start(i), timeRanges.end(i)]);

// Whether the stream ended is what the player says, as an application would find out. hls.js
// leaves it to the element's `ended` event. dash.js has its own `playbackEnded`: a timer of its
// own can decide the stream is over before the element says so, and then moves the position to
// 0.5 s before the end (its `seekDurationBackoff`) and pauses, so the element's `ended` never fires.
let everEnded = false;
video.addEventListener('ended', () => {
  everEnded = true;
});

video.addEventListener('error', () => errors.push(`video element: ${video.error && video.error.message}`));

// hls.js reports many conditions that a stream plays through. These are the ones that mean the
// stream itself is wrong: a request that failed, bytes the browser would not take, or a
// codec it cannot play. Any fatal error counts too.
const HLS_BROKEN = /Load|Parsing|Incompatible|Append|Codec|Key|Decrypt/;

window.harness = {
  errors,

  /**
   * Starts playing `url` with `kind`, either 'hls' or 'dash'. `start` is the rendition to begin on,
   * 'lowest' or 'highest'; the player's own ABR is switched off so that it stays there.
   */
  load(kind, url, start = 'lowest') {
    if (kind === 'hls') {
      hls = new Hls();
      hls.on(Hls.Events.ERROR, (_, data) => {
        if (data.fatal || HLS_BROKEN.test(data.details)) {
          errors.push(`hls.js ${data.details}${data.fatal ? ' (fatal)' : ''}`);
        }
      });
      // The player's own ABR must not move the rendition under a spec. Pinning the level once the
      // manifest is known turns it off; a spec changes the rendition itself with `setLevel`.
      hls.on(Hls.Events.MANIFEST_PARSED, () => {
        hls.currentLevel = start === 'highest' ? hls.levels.length - 1 : 0;
      });
      hls.loadSource(url);
      hls.attachMedia(video);
    } else {
      dash = dashjs.MediaPlayer().create();
      // A spec that forces a rendition must not have the player's ABR move it back, and the first
      // segments come from the rendition asked for: with ABR off dash.js starts on the one nearest
      // its initial bitrate. The buffer is
      // kept short because the assets last a few seconds: dash.js replaces only what is within its
      // buffer target on a switch, and its default target would hold the whole asset at once.
      dash.updateSettings({ streaming: { abr: { autoSwitchBitrate: { video: false }, initialBitrate: { video: start === 'highest' ? 100000 : 1 } }, buffer: { bufferTimeDefault: 1.5, bufferTimeAtTopQuality: 1.5, bufferTimeAtTopQualityLongForm: 1.5, fastSwitchEnabled: true } } });
      dash.on('error', (event) => errors.push(`dash.js ${JSON.stringify(event.error)}`));
      dash.on('playbackEnded', () => {
        everEnded = true;
      });
      dash.initialize(video, url, true);
    }
    video.play().catch(() => {});
  },

  state() {
    return {
      time: video.currentTime,
      duration: video.duration,
      ended: video.ended,
      everEnded,
      paused: video.paused,
      readyState: video.readyState,
      width: video.videoWidth,
      height: video.videoHeight,
      played: ranges(video.played),
    };
  },

  /** Seeks the way an application of that player does: dash.js is told, not the element. */
  seek(seconds) {
    if (dash) {
      dash.seek(seconds);
    } else {
      video.currentTime = seconds;
    }
  },

  /** The player instance, for a spec that needs to look inside it. */
  player() {
    return hls ?? dash;
  },

  pause() {
    video.pause();
  },

  resume() {
    return video.play();
  },

  /** The renditions of the video, in the order the player lists them. */
  levels() {
    return hls
      ? hls.levels.map((level) => ({ height: level.height, bitrate: level.bitrate }))
      : dash.getRepresentationsByType('video').map((r) => ({ height: r.height, bitrate: r.bandwidth }));
  },

  /** Switches to the rendition at `index` now, replacing what is buffered. */
  setLevel(index) {
    if (hls) {
      hls.currentLevel = index;
    } else {
      dash.setRepresentationForTypeByIndex('video', index, true);
    }
  },
};
