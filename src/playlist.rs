use crate::commands::Quality;
use anyhow::{bail, Result};
use serde::Serialize;

#[derive(Serialize)]
pub(crate) enum MediaType {
    Audio,
    Subtitles,
    Undefined,
    Video,
}

#[derive(Serialize)]
pub(crate) enum PlaylistType {
    Dash,
    Hls,
}

#[derive(Serialize)]
pub(crate) struct ByteRange {
    pub(crate) length: u64,
    pub(crate) offset: Option<u64>,
}

#[derive(Serialize)]
pub(crate) struct Map {
    pub(crate) uri: String,
    pub(crate) byte_range: Option<ByteRange>,
}

#[derive(Serialize)]
pub(crate) enum KeyMethod {
    Aes128,
    Cenc,
    None,
    Other(String),
    SampleAes,
}

#[derive(Serialize)]
pub(crate) struct Key {
    pub(crate) default_kid: Option<String>,
    pub(crate) iv: Option<String>,
    pub(crate) key_format: Option<String>,
    pub(crate) method: KeyMethod,
    pub(crate) uri: String,
}

#[derive(Default, Serialize)]
pub(crate) struct Segment {
    pub(crate) byte_range: Option<ByteRange>,
    // TODO - Support #EXT-X-DISCOUNTINUITY tag
    // pub(crate) discountinuity: bool,
    pub(crate) duration: f32,
    pub(crate) key: Option<Key>,
    pub(crate) map: Option<Map>,
    pub(crate) uri: String,
}

impl Segment {
    pub(crate) fn seg_url(&self, baseurl: &str) -> Result<reqwest::Url> {
        if self.uri.starts_with("http") || self.uri.starts_with("ftp") {
            Ok(self.uri.parse::<reqwest::Url>()?)
        } else {
            Ok(baseurl.parse::<reqwest::Url>()?.join(&self.uri)?)
        }
    }

    pub(crate) fn map_url(&self, baseurl: &str) -> Result<Option<reqwest::Url>> {
        if let Some(map) = &self.map {
            if self.uri.starts_with("http") || self.uri.starts_with("ftp") {
                return Ok(Some(map.uri.parse::<reqwest::Url>()?));
            } else {
                return Ok(Some(baseurl.parse::<reqwest::Url>()?.join(&map.uri)?));
            }
        }

        Ok(None)
    }

    pub(crate) fn key_url(&self, baseurl: &str) -> Result<Option<reqwest::Url>> {
        if let Some(key) = &self.key {
            if self.uri.starts_with("http") || self.uri.starts_with("ftp") {
                return Ok(Some(key.uri.parse::<reqwest::Url>()?));
            } else {
                return Ok(Some(baseurl.parse::<reqwest::Url>()?.join(&key.uri)?));
            }
        }

        Ok(None)
    }

    pub(crate) fn seg_range(&self, previous_byterange_end: u64) -> Option<(String, u64)> {
        if let Some(byte_range) = &self.byte_range {
            let offset = byte_range.offset.unwrap_or(0);

            let (start, end) = if offset == 0 {
                (
                    previous_byterange_end,
                    previous_byterange_end + byte_range.length - 1,
                )
            } else {
                (byte_range.length, byte_range.length + offset - 1)
            };

            Some((format!("bytes={}-{}", start, end), end))
        } else {
            None
        }
    }

    pub(crate) fn map_range(&self, previous_byterange_end: u64) -> Option<(String, u64)> {
        if let Some(map) = &self.map {
            if let Some(byte_range) = &map.byte_range {
                let offset = byte_range.offset.unwrap_or(0);

                let (start, end) = if offset == 0 {
                    (
                        previous_byterange_end,
                        previous_byterange_end + byte_range.length - 1,
                    )
                } else {
                    (byte_range.length, byte_range.length + offset - 1)
                };

                return Some((format!("bytes={}-{}", start, end), end));
            }
        }

        None
    }
}

#[derive(Serialize)]
pub(crate) struct MediaPlaylist {
    pub(crate) bandwidth: Option<u64>,
    pub(crate) channels: Option<f32>,
    pub(crate) codecs: Option<String>,
    pub(crate) extension: Option<String>,
    pub(crate) frame_rate: Option<f32>,
    pub(crate) i_frame: bool,
    pub(crate) language: Option<String>,
    pub(crate) live: bool,
    pub(crate) media_type: MediaType,
    pub(crate) playlist_type: PlaylistType,
    pub(crate) resolution: Option<(u64, u64)>,
    pub(crate) segments: Vec<Segment>,
    pub(crate) uri: String,
}

impl MediaPlaylist {
    fn has_resolution(&self, w: u16, h: u16) -> bool {
        if let Some((video_w, video_h)) = self.resolution {
            w as u64 == video_w && h as u64 == video_h
        } else {
            false
        }
    }

    fn display_video_stream(&self) -> String {
        let resolution = if let Some((w, h)) = self.resolution {
            match (w, h) {
                (256, 144) => "144p".to_owned(),
                (426, 240) => "240p".to_owned(),
                (640, 360) => "360p".to_owned(),
                (854, 480) => "480p".to_owned(),
                (1280, 720) => "720p".to_owned(),
                (1920, 1080) => "1080p".to_owned(),
                (2048, 1080) => "2K".to_owned(),
                (2560, 1440) => "1440p".to_owned(),
                (3840, 2160) => "4K".to_owned(),
                (7680, 4320) => "8K".to_owned(),
                (w, h) => format!("{}x{}", w, h),
            }
        } else {
            "?".to_owned()
        };

        let bandwidth = if let Some(bandwidth) = self.bandwidth {
            crate::utils::format_bytes(bandwidth as usize, 2)
        } else {
            ("?".to_owned(), "?".to_owned(), "?".to_owned())
        };

        let mut extra = format!("(codecs: {}", self.codecs.unwrap_or("?".to_owned()));

        if let Some(frame_rate) = self.frame_rate {
            extra += &format!(", frame_rate: {}", frame_rate);
        }

        if self.i_frame {
            extra += ", iframe";
        }

        if self.live {
            extra += ", live";
        }

        extra += ")";

        format!(
            "{:9} {:>6} {}/s {}",
            resolution, bandwidth.0, bandwidth.1, extra
        )
    }

    fn display_audio_stream(&self) -> String {
        let mut extra = format!("language: {}", self.language.unwrap_or("?".to_owned()));

        if let Some(codecs) = self.codecs {
            extra += &format!(", codecs: {}", codecs);
        }

        if let Some(bandwidth) = self.bandwidth {
            extra += &format!(
                ", bandwidth: {}/s",
                crate::utils::format_bytes(bandwidth as usize, 2).2
            );
        }

        if let Some(channels) = self.channels {
            extra += &format!(", channels: {}", channels);
        }

        if self.live {
            extra += ", live";
        }

        extra
    }

    fn display_subtitles_stream(&self) -> String {
        let mut extra = format!("language: {}", self.language.unwrap_or("?".to_owned()));

        if let Some(codecs) = self.codecs {
            extra += &format!(", codecs: {}", codecs);
        }

        extra
    }

    pub(crate) fn url(&self, baseurl: &str) -> Result<reqwest::Url> {
        // self.uri.starts_with("dash://")
        if self.uri.starts_with("http") || self.uri.starts_with("ftp") {
            Ok(self.uri.parse::<reqwest::Url>()?)
        } else {
            Ok(baseurl.parse::<reqwest::Url>()?.join(&self.uri)?)
        }
    }

    pub(crate) fn is_hls(&self) -> bool {
        match &self.playlist_type {
            PlaylistType::Hls => true,
            _ => false,
        }
    }

    pub(crate) fn is_dash(&self) -> bool {
        match &self.playlist_type {
            PlaylistType::Dash => true,
            _ => false,
        }
    }

    pub(crate) fn extension(&self) -> String {
        if let Some(ext) = &self.extension {
            return ext.to_owned();
        }

        let mut ext = match &self.playlist_type {
            PlaylistType::Hls => "ts",
            PlaylistType::Dash => "m4s",
        };

        if let Some(segment) = self.segments.get(0) {
            if let Some(init) = segment.map {
                if init.uri.ends_with(".mp4") {
                    ext = "m4s";
                }
            }

            if segment.uri.ends_with(".mp4") {
                ext = "mp4";
            }
        }

        ext.to_owned()
    }
}

#[derive(Serialize)]
pub(crate) struct MasterPlaylist {
    pub(crate) playlist_type: PlaylistType,
    pub(crate) uri: String,
    pub(crate) streams: Vec<MediaPlaylist>,
}

impl MasterPlaylist {
    pub(crate) fn url(&self, baseurl: &str) -> Result<reqwest::Url> {
        if self.uri.starts_with("http") || self.uri.starts_with("ftp") {
            Ok(self.uri.parse::<reqwest::Url>()?)
        } else {
            Ok(baseurl.parse::<reqwest::Url>()?.join(&self.uri)?)
        }
    }

    pub(crate) fn is_hls(&self) -> bool {
        match self.playlist_type {
            PlaylistType::Hls => true,
            _ => false,
        }
    }

    pub(crate) fn is_dash(&self) -> bool {
        match self.playlist_type {
            PlaylistType::Dash => true,
            _ => false,
        }
    }

    pub(crate) fn sort_streams(
        &mut self,
        prefer_audio_lang: Option<String>,
        prefer_subs_lang: Option<String>,
    ) {
        let prefer_audio_lang = prefer_audio_lang.map(|x| x.to_lowercase());
        let prefer_subs_lang = prefer_subs_lang.map(|x| x.to_lowercase());

        let mut video_streams = self
            .streams
            .into_iter()
            .filter_map(|x| {
                if matches!(x.media_type, MediaType::Video) {
                    let pixels = if let Some((w, h)) = &x.resolution {
                        w * h
                    } else {
                        0
                    };

                    Some((x, pixels, x.bandwidth.unwrap_or(0)))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        video_streams.sort_by(|x, y| y.2.cmp(&x.2));
        video_streams.sort_by(|x, y| y.1.cmp(&x.1));

        let mut audio_streams = self
            .streams
            .into_iter()
            .filter_map(|x| {
                if matches!(x.media_type, MediaType::Audio) {
                    let mut language_factor = 0;

                    if let Some(playlist_lang) = x.language.as_ref().map(|x| x.to_lowercase()) {
                        if let Some(prefer_lang) = prefer_audio_lang {
                            if playlist_lang == prefer_lang {
                                language_factor = 2;
                            } else if playlist_lang.get(0..2) == prefer_lang.get(0..2) {
                                language_factor = 1;
                            }
                        }
                    }

                    Some((
                        x,
                        language_factor,
                        x.channels.unwrap_or(0.0),
                        x.bandwidth.unwrap_or(0),
                    ))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        audio_streams.sort_by(|x, y| y.3.cmp(&x.3));
        audio_streams.sort_by(|x, y| y.2.total_cmp(&x.2));
        audio_streams.sort_by(|x, y| y.1.cmp(&x.1));

        let mut subtitles_streams = self
            .streams
            .into_iter()
            .filter_map(|x| {
                if matches!(x.media_type, MediaType::Subtitles) {
                    let mut language_factor = 0;

                    if let Some(playlist_lang) = x.language.as_ref().map(|x| x.to_lowercase()) {
                        if let Some(prefer_lang) = prefer_subs_lang {
                            if playlist_lang == prefer_lang {
                                language_factor = 2;
                            } else if playlist_lang.get(0..2) == prefer_lang.get(0..2) {
                                language_factor = 1;
                            }
                        }
                    }

                    Some((x, language_factor))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        subtitles_streams.sort_by(|x, y| y.1.cmp(&x.1));

        self.streams = video_streams
            .into_iter()
            .map(|x| x.0)
            .chain(audio_streams.into_iter().map(|x| x.0))
            .chain(subtitles_streams.into_iter().map(|x| x.0))
            .collect::<Vec<_>>();
    }

    // pub(crate) fn select_stream(&self, quality: &Quality, raw_prompts: bool) -> Result<MediaPlaylist> {

    // }

    // https://docs.rs/requestty/latest/requestty/question/struct.Question.html#method.select
    // TODO - Raw prompts
    pub(crate) fn select_streams(&self, quality: &Quality) -> Result<()> {
        let audio_streams = self
            .streams
            .iter()
            .filter(|x| matches!(x.media_type, MediaType::Audio))
            .collect::<Vec<_>>();
        let subtitles_streams = self
            .streams
            .iter()
            .filter(|x| matches!(x.media_type, MediaType::Subtitles))
            .collect::<Vec<_>>();

        let video_streams = self
            .streams
            .iter()
            .filter(|x| matches!(x.media_type, MediaType::Video));

        let default_video_stream = match quality {
            Quality::Lowest => video_streams.last(),
            Quality::Highest | Quality::SelectLater => video_streams.next(),
            Quality::Resolution(w, h) => video_streams.find(|x| x.has_resolution(*w, *h)),
            Quality::Youtube144p => video_streams.find(|x| x.has_resolution(256, 144)),
            Quality::Youtube240p => video_streams.find(|x| x.has_resolution(426, 240)),
            Quality::Youtube360p => video_streams.find(|x| x.has_resolution(640, 360)),
            Quality::Youtube480p => video_streams.find(|x| x.has_resolution(854, 480)),
            Quality::Youtube720p => video_streams.find(|x| x.has_resolution(1280, 720)),
            Quality::Youtube1080p => video_streams.find(|x| x.has_resolution(1920, 1080)),
            Quality::Youtube2k => video_streams.find(|x| x.has_resolution(2048, 1080)),
            Quality::Youtube1440p => video_streams.find(|x| x.has_resolution(2560, 1440)),
            Quality::Youtube4k => video_streams.find(|x| x.has_resolution(3840, 2160)),
            Quality::Youtube8k => video_streams.find(|x| x.has_resolution(7680, 4320)),
        };

        if let Some(default_video_stream) = default_video_stream {
            let mut choices = vec![];
            choices.push(requestty::Separator(
                "─────── Video Streams ───────".to_owned(),
            ));
            choices.extend(self.streams.iter().filter_map(|x| {
                if matches!(x.media_type, MediaType::Video) {
                    Some(requestty::Choice(x.display_video_stream()))
                } else {
                    None
                }
            }));
            choices.push(requestty::Separator(
                "─────── Audio Streams ───────".to_owned(),
            ));
            choices.extend(self.streams.iter().filter_map(|x| {
                if matches!(x.media_type, MediaType::Audio) {
                    Some(requestty::Choice(x.display_audio_stream()))
                } else {
                    None
                }
            }));
            choices.push(requestty::Separator(
                "───── Subtitles Streams ─────".to_owned(),
            ));
            choices.extend(self.streams.iter().filter_map(|x| {
                if matches!(x.media_type, MediaType::Subtitles) {
                    Some(requestty::Choice(x.display_subtitles_stream()))
                } else {
                    None
                }
            }));

            let mut choices_with_default =
                vec![(requestty::Choice((default_video_stream.display_video_stream(), true)))];

            if let Some(x) = self
                .streams
                .iter()
                .find(|x| matches!(x.media_type, MediaType::Audio))
            {
                choices_with_default.push(requestty::Choice((x.display_audio_stream(), true)));
            }

            if let Some(x) = self
                .streams
                .iter()
                .find(|x| matches!(x.media_type, MediaType::Subtitles))
            {
                choices_with_default.push(requestty::Choice((x.display_subtitles_stream(), true)));
            }

            let question = requestty::Question::multi_select("streams")
                .should_loop(false)
                .message("Select streams to download")
                .choices_with_default(choices_with_default)
                .choices(choices)
                // .validate(filter);
                .build();

            let answer = requestty::prompt_one(question)?;
            println!("{:#?}", answer);

            Ok(())
        } else {
            // TODO - Add better message
            // Selected variant stream of quality {} ({} {}/s).
            bail!("playlist doesn't contain {:?} quality stream", quality)
        }
    }
}
