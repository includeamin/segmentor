//! Which tracks of a file to serve (TDD 0015): nginx-vod-module's `tracks` syntax.

use super::{Track, TrackKey, TrackKind};
use crate::error::{Error, Result};

const MAX_BYTES: usize = 32;

/// What to take of one kind of track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pick {
    /// The kind is not mentioned, so none of it is served.
    Nothing,
    /// `v0` or `a0`: every track of the kind.
    All,
    /// `v2`, `a1`: the nth track of the kind in file order, counting from one.
    Nth(u8),
}

/// A `tracks` selector such as `v1-a1`, `a2`, or `v0-a0`.
///
/// `v1-a1`, nginx-vod-module's default, serves the first video and the first audio track of a
/// file and nothing else. A selector that matches no track of a kind selects nothing of that
/// kind, so `v1-a1` still serves a file that has no audio; only a selection that leaves no track
/// at all is an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TrackSelection {
    video: Pick,
    audio: Pick,
}

impl TrackSelection {
    /// Reads the selector. The reason names the problem and never needs more than the text.
    pub(crate) fn parse(text: &str) -> std::result::Result<Self, String> {
        let invalid =
            || format!("tracks must look like v1, a2, v1-a1, or v0-a0 (at most {MAX_BYTES} bytes)");
        if text.is_empty() || text.len() > MAX_BYTES {
            return Err(invalid());
        }
        let mut selection = Self {
            video: Pick::Nothing,
            audio: Pick::Nothing,
        };
        for part in text.split('-') {
            let (kind, digits) = part.split_at_checked(1).ok_or_else(invalid)?;
            if digits.is_empty() || digits.len() > 3 || !digits.bytes().all(|b| b.is_ascii_digit())
            {
                return Err(invalid());
            }
            let number: u8 = digits.parse().map_err(|_| invalid())?;
            let pick = if number == 0 {
                Pick::All
            } else {
                Pick::Nth(number)
            };
            let slot = match kind {
                "v" => &mut selection.video,
                "a" => &mut selection.audio,
                _ => return Err(invalid()),
            };
            if *slot != Pick::Nothing {
                return Err(format!(
                    "tracks names the same kind of track twice ({text})"
                ));
            }
            *slot = pick;
        }
        Ok(selection)
    }

    /// The selector in one spelling, whatever the mapper wrote: what a URL version hashes.
    pub(crate) fn canonical(self) -> String {
        let part = |letter: char, pick: Pick| match pick {
            Pick::Nothing => None,
            Pick::All => Some(format!("{letter}0")),
            Pick::Nth(number) => Some(format!("{letter}{number}")),
        };
        [part('v', self.video), part('a', self.audio)]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join("-")
    }

    /// The tracks of a file that the selector takes, in file order, with the audio tracks
    /// numbered `audio-1`, `audio-2`… afresh. Fails when nothing is left.
    pub(crate) fn apply(self, tracks: Vec<Track>) -> Result<Vec<Track>> {
        let (mut videos, mut audios) = (0u16, 0u16);
        let mut kept: Vec<Track> = tracks
            .into_iter()
            .filter(|track| {
                let (seen, pick) = match track.kind {
                    TrackKind::Video => (&mut videos, self.video),
                    TrackKind::Audio => (&mut audios, self.audio),
                };
                *seen += 1;
                match pick {
                    Pick::Nothing => false,
                    Pick::All => true,
                    Pick::Nth(number) => *seen == u16::from(number),
                }
            })
            .collect();
        if kept.is_empty() {
            return Err(Error::InvalidMedia(format!(
                "tracks `{}` selects no track of this file",
                self.canonical()
            )));
        }
        let mut audio = 0u16;
        for track in &mut kept {
            track.key = match track.kind {
                TrackKind::Video => TrackKey::VIDEO,
                TrackKind::Audio => {
                    audio += 1;
                    TrackKey::audio(audio)
                }
            };
        }
        Ok(kept)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{CodecConfig, SampleIndex};

    fn track(id: u32, kind: TrackKind, key: TrackKey) -> Track {
        Track {
            id,
            key,
            kind,
            language: "und".to_owned(),
            timeline_shift: 0,
            timescale: 1000,
            duration: 0,
            codec: CodecConfig::Aac {
                sample_rate: 48_000,
                channels: 2,
                object_type: 2,
            },
            samples: SampleIndex::from_samples(&[]),
            source_protection: None,
        }
    }

    /// A video track and three audio tracks, as a file lists them.
    fn file() -> Vec<Track> {
        vec![
            track(1, TrackKind::Video, TrackKey::VIDEO),
            track(2, TrackKind::Audio, TrackKey::audio(1)),
            track(3, TrackKind::Audio, TrackKey::audio(2)),
            track(4, TrackKind::Audio, TrackKey::audio(3)),
        ]
    }

    fn ids(tracks: &[Track]) -> Vec<u32> {
        tracks.iter().map(|track| track.id).collect()
    }

    fn keys(tracks: &[Track]) -> Vec<String> {
        tracks.iter().map(|track| track.key.to_string()).collect()
    }

    #[test]
    fn the_selectors_mappers_write_are_read() {
        for (text, canonical) in [
            ("v1-a1", "v1-a1"),
            ("a1-v1", "v1-a1"),
            ("v1", "v1"),
            ("a2", "a2"),
            ("v0-a0", "v0-a0"),
            ("a0", "a0"),
            ("v01-a001", "v1-a1"),
            ("a255", "a255"),
        ] {
            let selection = TrackSelection::parse(text).unwrap_or_else(|e| panic!("{text}: {e}"));
            assert_eq!(selection.canonical(), canonical, "{text}");
        }
    }

    #[test]
    fn anything_else_is_refused_with_the_rule_and_without_a_panic() {
        for text in [
            "", "x1", "v", "a-", "-a1", "v1-", "v1--a1", "v1-v2", "a1-a2", "v1a1", "V1", "v 1",
            "v-1", "v+1", "v1.5", "a256", "a9999", "v1-a1-s1", "v１",
        ] {
            let error = TrackSelection::parse(text).unwrap_err();
            assert!(error.contains("tracks"), "{text}: {error}");
        }
        assert!(TrackSelection::parse(&"v1-".repeat(20)).is_err());
    }

    #[test]
    fn the_nth_track_of_a_kind_is_taken_in_file_order_and_renumbered() {
        let selection = TrackSelection::parse("v1-a2").unwrap();
        let kept = selection.apply(file()).unwrap();
        assert_eq!(ids(&kept), [1, 3]);
        assert_eq!(
            keys(&kept),
            ["video", "audio-1"],
            "the second audio is now the first"
        );

        let kept = TrackSelection::parse("a3").unwrap().apply(file()).unwrap();
        assert_eq!(ids(&kept), [4]);
        assert_eq!(keys(&kept), ["audio-1"]);
    }

    #[test]
    fn a_zero_takes_every_track_of_the_kind_and_a_kind_not_named_is_dropped() {
        let kept = TrackSelection::parse("v1-a0")
            .unwrap()
            .apply(file())
            .unwrap();
        assert_eq!(ids(&kept), [1, 2, 3, 4]);
        assert_eq!(keys(&kept), ["video", "audio-1", "audio-2", "audio-3"]);

        let kept = TrackSelection::parse("a0").unwrap().apply(file()).unwrap();
        assert_eq!(ids(&kept), [2, 3, 4]);
        assert_eq!(keys(&kept), ["audio-1", "audio-2", "audio-3"]);

        let kept = TrackSelection::parse("v1").unwrap().apply(file()).unwrap();
        assert_eq!(ids(&kept), [1]);
    }

    #[test]
    fn a_selector_that_matches_nothing_of_a_kind_still_serves_the_rest() {
        // The default `v1-a1` on a file with no audio, and on one with audio only.
        let video_only = vec![track(1, TrackKind::Video, TrackKey::VIDEO)];
        let kept = TrackSelection::parse("v1-a1")
            .unwrap()
            .apply(video_only)
            .unwrap();
        assert_eq!(ids(&kept), [1]);
        let audio_only = vec![track(5, TrackKind::Audio, TrackKey::audio(1))];
        let kept = TrackSelection::parse("v1-a1")
            .unwrap()
            .apply(audio_only)
            .unwrap();
        assert_eq!(ids(&kept), [5]);
        // `a9` takes none of the three audio tracks, but the video is still there.
        let kept = TrackSelection::parse("v1-a9")
            .unwrap()
            .apply(file())
            .unwrap();
        assert_eq!(ids(&kept), [1]);
    }

    #[test]
    fn a_selection_that_leaves_nothing_is_an_error_that_names_it() {
        let error = TrackSelection::parse("a9")
            .unwrap()
            .apply(file())
            .unwrap_err();
        assert!(
            error.to_string().contains("tracks `a9` selects no track"),
            "{error}"
        );
        let error = TrackSelection::parse("v2-a2")
            .unwrap()
            .apply(vec![track(1, TrackKind::Video, TrackKey::VIDEO)])
            .unwrap_err();
        assert!(error.to_string().contains("`v2-a2`"), "{error}");
        assert!(
            TrackSelection::parse("v1")
                .unwrap()
                .apply(Vec::new())
                .is_err()
        );
    }
}
