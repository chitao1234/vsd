use crate::{
    commands::Quality,
    merger::Merger,
    playlist::{KeyMethod, MediaType, PlaylistType},
    utils,
};
use anyhow::{anyhow, bail, Result};
use kdam::{term::Colorizer, tqdm, BarExt, Column, RichProgress};
use reqwest::{
    blocking::{Client, RequestBuilder},
    header::{CONTENT_TYPE, RANGE},
    StatusCode, Url,
};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs::File,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    time::Instant,
};

enum SubtitleType {
    Mp4Vtt,
    Mp4Ttml,
    SrtText,
    TtmlText,
    VttText,
}

struct Stream {
    file_path: String,
    language: Option<String>,
    media_type: MediaType,
}

#[derive(Clone)]
struct Keys {
    bytes: Vec<u8>,
    iv: Option<String>,
    method: KeyMethod,
}

impl Keys {
    fn from_hex_keys(keys: HashMap<String, String>) -> Self {
        let mut bytes = String::new();

        for (kid, key) in keys {
            bytes += &(kid + ":" + &key + ";");
        }

        Self {
            bytes: bytes.get(..(bytes.len() - 1)).unwrap().as_bytes().to_vec(),
            iv: None,
            method: KeyMethod::Cenc,
        }
    }

    fn as_hex_keys(&self) -> HashMap<String, String> {
        String::from_utf8(self.bytes.clone())
            .unwrap()
            .split(';')
            .map(|x| {
                let kid_key = x.split(':').collect::<Vec<_>>();
                (
                    kid_key.get(0).unwrap().to_string(),
                    kid_key.get(1).unwrap().to_string(),
                )
            })
            .collect()
    }

    fn decrypt(&self, data: Vec<u8>) -> Result<Vec<u8>> {
        Ok(match self.method {
            KeyMethod::Aes128 => openssl::symm::decrypt(
                openssl::symm::Cipher::aes_128_cbc(),
                &self.bytes,
                self.iv.as_ref().map(|x| x.as_bytes()),
                &data,
            )?,
            KeyMethod::Cenc | KeyMethod::SampleAes => {
                mp4decrypt::mp4decrypt(&data, self.as_hex_keys(), None).map_err(|x| anyhow!(x))?
            }
            _ => data,
        })
    }
}

pub(crate) fn download(
    all_keys: bool,
    baseurl: Option<Url>,
    client: Client,
    directory: Option<PathBuf>,
    input: &str,
    keys: Vec<(Option<String>, String)>,
    no_decrypt: bool,
    output: Option<String>,
    prefer_audio_lang: Option<String>,
    prefer_subs_lang: Option<String>,
    quality: Quality,
    skip_prompts: bool,
    raw_prompts: bool,
    retry_count: u8,
    threads: u8,
) -> Result<()> {
    let mut playlist_url = "https://example.com".parse::<Url>().unwrap();

    // -----------------------------------------------------------------------------------------
    // Fetch Playlist
    // -----------------------------------------------------------------------------------------

    let mut playlist_type = None;
    let path = Path::new(input);

    let playlist = if path.exists() {
        if baseurl.is_none() {
            println!(
                "    {} base url is not set",
                "Warning".colorize("bold yellow")
            );
        }

        if let Some(ext) = path.extension() {
            let ext = ext.to_string_lossy();
            if ext == "mpd" {
                playlist_type = Some(PlaylistType::Dash);
            } else if ext == "m3u" || ext == "m3u8" {
                playlist_type = Some(PlaylistType::Hls);
            }
        }

        if playlist_type.is_some() {
            std::fs::read(path)?
        } else {
            let text = std::fs::read_to_string(path)?;
            if text.contains("<MPD") {
                playlist_type = Some(PlaylistType::Dash);
            } else if text.contains("#EXTM3U") {
                playlist_type = Some(PlaylistType::Hls);
            }
            text.as_bytes().to_vec()
        }
    } else {
        let response = client.get(input).send()?;
        playlist_url = response.url().to_owned();

        if let Some(content_type) = response.headers().get(CONTENT_TYPE) {
            match content_type.as_bytes() {
                b"application/dash+xml" | b"video/vnd.mpeg.dash.mpd" => {
                    playlist_type = Some(PlaylistType::Dash)
                }
                b"application/x-mpegurl" | b"application/vnd.apple.mpegurl" => {
                    playlist_type = Some(PlaylistType::Hls)
                }
                _ => (),
            }
        }

        if playlist_type.is_none() {
            let text = response.text()?;
            if text.contains("<MPD") {
                playlist_type = Some(PlaylistType::Dash);
            } else if text.contains("#EXTM3U") {
                playlist_type = Some(PlaylistType::Hls);
            }
            text.as_bytes().to_vec()
        } else {
            response.bytes()?.to_vec()
        }
    };

    // -----------------------------------------------------------------------------------------
    // Parse Playlist & Select Streams & Push Segments
    // -----------------------------------------------------------------------------------------

    let (video_audio_streams, subtitle_streams) = match playlist_type {
        Some(PlaylistType::Dash) => {
            let playlist = String::from_utf8(playlist).unwrap(); // TODO - Return error
            let mpd = dash_mpd::parse(&playlist).map_err(|x| {
                anyhow!(
                    "couldn't parse xml string as mpd content (failed with {}).\n\n{}",
                    x,
                    playlist
                )
            })?;
            let (mut video_audio_streams, mut subtitle_streams) =
                crate::dash::parse_as_master(&mpd, playlist_url.as_str())
                    .sort_streams(prefer_audio_lang, prefer_subs_lang)
                    .select_streams(quality, skip_prompts, raw_prompts)?;

            for stream in video_audio_streams
                .iter_mut()
                .chain(subtitle_streams.iter_mut())
            {
                crate::dash::push_segments(
                    &mpd,
                    stream,
                    baseurl.as_ref().unwrap_or(&playlist_url).as_str(),
                )?;
                stream.uri = playlist_url.as_str().to_owned();
            }

            (video_audio_streams, subtitle_streams)
        }
        Some(PlaylistType::Hls) => match m3u8_rs::parse_playlist_res(&playlist) {
            Ok(m3u8_rs::Playlist::MasterPlaylist(m3u8)) => {
                let (mut video_audio_streams, mut subtitle_streams) =
                    crate::hls::parse_as_master(&m3u8, playlist_url.as_str())
                        .sort_streams(prefer_audio_lang, prefer_subs_lang)
                        .select_streams(quality, skip_prompts, raw_prompts)?;

                for stream in video_audio_streams
                    .iter_mut()
                    .chain(subtitle_streams.iter_mut())
                {
                    stream.uri = stream
                        .url(baseurl.as_ref().unwrap_or(&playlist_url))?
                        .as_str()
                        .to_owned();
                    let response = client.get(&stream.uri).send()?;
                    let media_playlist =
                        m3u8_rs::parse_media_playlist_res(&response.bytes()?).unwrap(); // TODO - Add better message for error
                    crate::hls::push_segments(&media_playlist, stream);
                }

                (video_audio_streams, subtitle_streams)
            }
            Ok(m3u8_rs::Playlist::MediaPlaylist(m3u8)) => {
                let mut media_playlist = crate::playlist::MediaPlaylist::default();
                media_playlist.uri = playlist_url.as_str().to_owned();
                crate::hls::push_segments(&m3u8, &mut media_playlist);
                (vec![media_playlist], vec![])
            }
            Err(_) => bail!("couldn't parse {} as HLS playlist.", playlist_url),
        },
        _ => bail!("only DASH (.mpd) and HLS (.m3u8) playlists are supported."),
    };

    // -----------------------------------------------------------------------------------------
    // Parse Key Ids
    // -----------------------------------------------------------------------------------------

    let mut default_kids = HashSet::new();

    for stream in &video_audio_streams {
        if let Some(segment) = stream.segments.get(0) {
            if let Some(key) = &segment.key {
                if !no_decrypt {
                    match &key.method {
                        KeyMethod::Other(x) => bail!("{} decryption is not supported. Use {} flag to download encrypted streams.", x, "--no-decrypt".colorize("bold green")),
                        KeyMethod::SampleAes => {
                            if stream.is_hls() {
                                // TODO - Only if "keyformat=identity" 
                                bail!("sample-aes (HLS) decryption is not supported. Use {} flag to download encrypted streams.", "--no-decrypt".colorize("bold green"));
                            }
                        }
                        _ => (),
                    }
                }

                if let Some(default_kid) = &key.default_kid {
                    default_kids.insert(default_kid.replace('-', ""));
                }
            }
        }
    }

    let mut kids = HashSet::new();

    for stream in &video_audio_streams {
        if let Some(segment) = stream.segments.get(0) {
            if segment.map.is_some() {
                let mut request = client.get(
                    segment
                        .map_url(
                            baseurl
                                .as_ref()
                                .unwrap_or(&stream.uri.parse::<Url>().unwrap()),
                        )?
                        .unwrap(),
                );

                if let Some((range, _)) = segment.map_range(0) {
                    request = request.header(RANGE, range);
                }

                let response = request.send()?;
                let pssh = crate::mp4parser::Pssh::new(&response.bytes()?).unwrap(); // TODO - Add better message for error

                for key_id in pssh.key_ids {
                    if !kids.contains(&key_id.value) {
                        kids.insert(key_id.value.clone());
                        println!(
                            "      {} {} {} ({})",
                            "KeyId".colorize("bold green"),
                            if default_kids.contains(&key_id.value) {
                                "*"
                            } else {
                                " "
                            },
                            key_id.uuid(),
                            key_id.system_type,
                        );
                    }
                }
            }
        }
    }

    for default_kid in &default_kids {
        if !keys
            .iter()
            .flat_map(|x| x.0.as_ref())
            .any(|x| x == default_kid)
            && !no_decrypt
        {
            bail!(
                "use {} flag to specify CENC content decryption keys for at least * (star) prefixed key ids.",
                "--key".colorize("bold green")
            );
        }
    }

    // -----------------------------------------------------------------------------------------
    // Prepare Progress Bar
    // -----------------------------------------------------------------------------------------

    let mut pb = RichProgress::new(
        tqdm!(unit = " SEG".to_owned(), dynamic_ncols = true),
        vec![
            Column::text("[bold blue]?"),
            Column::Bar,
            Column::Percentage(0),
            Column::text("•"),
            Column::CountTotal,
            Column::text("•"),
            Column::ElapsedTime,
            Column::text("[cyan]>"),
            Column::RemainingTime,
            Column::text("•"),
            Column::Rate,
        ],
    );

    // -----------------------------------------------------------------------------------------
    // Prepare Directory & Store Streams Metadata
    // -----------------------------------------------------------------------------------------

    if let Some(directory) = &directory {
        if !directory.exists() {
            std::fs::create_dir_all(directory)?;
        }
    }

    if output.is_some() {
        let video_streams_count = video_audio_streams
            .iter()
            .filter(|x| x.media_type == MediaType::Video)
            .count();
        let audio_streams_count = video_audio_streams
            .iter()
            .filter(|x| x.media_type == MediaType::Audio)
            .count();

        if video_streams_count > 1 {
            println!(
                "    {} --output is ignored when multiple video streams are selected",
                "Warning".colorize("bold yellow")
            );
        }

        if video_streams_count == 0
            && (audio_streams_count > 1
                || subtitle_streams.len() > 1
                || (audio_streams_count != 0 && subtitle_streams.len() != 0))
        {
            println!(
                "    {} --output is ignored when no video streams is selected but multiple audio/subtitle streams are selected",
                "Warning".colorize("bold yellow")
            );
        }

        if no_decrypt {
            println!(
                "    {} --output is ignored when --no-decrypt is used",
                "Warning".colorize("bold yellow")
            );
        }
    }

    let mut temp_files = vec![];

    // -----------------------------------------------------------------------------------------
    // Download Subtitle Streams
    // -----------------------------------------------------------------------------------------

    for stream in subtitle_streams {
        pb.write(format!(
            " {} {} stream {}",
            "Processing".colorize("bold green"),
            stream.media_type,
            stream.display_stream().colorize("cyan"),
        ));

        let length = stream.segments.len();

        if length == 0 {
            pb.write(format!(
                "    {} skipping stream (no segments)",
                "Warning".colorize("bold yellow"),
            ));
            continue;
        }

        pb.pb.set_total(length);

        let mut ext = stream.extension();
        let mut codec = None;

        if let Some(codecs) = &stream.codecs {
            match codecs.as_str() {
                "vtt" => {
                    ext = "vtt".to_owned();
                    codec = Some(SubtitleType::VttText);
                }
                "wvtt" => {
                    ext = "vtt".to_owned();
                    codec = Some(SubtitleType::Mp4Vtt);
                }
                "stpp" | "stpp.ttml" | "stpp.ttml.im1t" | "stpp.TTML.im1t" => {
                    ext = "srt".to_owned();
                    codec = Some(SubtitleType::Mp4Ttml);
                }
                _ => (),
            }
        }

        let seg_baseurl = baseurl
            .clone()
            .unwrap_or(stream.uri.parse::<Url>().unwrap());

        let mut temp_file = String::new();
        let mut previous_byterange_end = 0;

        let mut first_run = true;
        let mut subtitles_data = vec![];

        for segment in &stream.segments {
            if segment.map.is_some() {
                let mut request = client.get(segment.map_url(&seg_baseurl)?.unwrap());

                if let Some((range, _)) = segment.map_range(0) {
                    request = request.header(RANGE, range);
                }

                let response = request.send()?;
                let bytes = response.bytes()?;
                subtitles_data.extend_from_slice(&bytes);
            }

            let mut request = client.get(segment.seg_url(&seg_baseurl)?);

            if let Some((range, previous)) = segment.seg_range(previous_byterange_end) {
                previous_byterange_end = previous;
                request = request.header(RANGE, range);
            }

            let response = request.send()?;
            let bytes = response.bytes()?;
            subtitles_data.extend_from_slice(&bytes);

            if first_run {
                first_run = false;

                if subtitles_data.starts_with(b"WEBVTT") {
                    ext = "vtt".to_owned();
                    codec = Some(SubtitleType::VttText);
                } else if subtitles_data.starts_with(b"1") {
                    ext = "srt".to_owned();
                    codec = Some(SubtitleType::SrtText);
                } else if subtitles_data.starts_with(b"<?xml") || subtitles_data.starts_with(b"<tt")
                {
                    // TODO - Match using Representation node @mimeType (DASH)
                    // application/ttml+xml
                    ext = "srt".to_owned();
                    codec = Some(SubtitleType::TtmlText);
                } else if codec.is_none() {
                    bail!("cannot determine subtitle codec.");
                }

                temp_file = stream
                    .file_path(&directory, &ext)
                    .to_string_lossy()
                    .to_string();
                temp_files.push(Stream {
                    file_path: temp_file.clone(),
                    language: stream.language.clone(),
                    media_type: stream.media_type.clone(),
                });
                pb.write(format!(
                    "{} stream to {}",
                    "Downloading".colorize("bold green"),
                    temp_file.colorize("cyan")
                ));
            }

            pb.replace(
                0,
                Column::Text(format!(
                    "[bold blue]{}",
                    utils::format_bytes(subtitles_data.len(), 2).2
                )),
            );
            pb.update(1);
        }

        match codec {
            Some(SubtitleType::Mp4Vtt) => {
                pb.write(format!(
                    " {} wvtt subtitles",
                    "Extracting".colorize("bold cyan"),
                ));

                let vtt = crate::mp4parser::Mp4VttParser::parse_init(&subtitles_data)
                    .map_err(|x| anyhow!(x))?;
                let cues = vtt
                    .parse_media(&subtitles_data, None)
                    .map_err(|x| anyhow!(x))?;
                let subtitles = crate::mp4parser::Subtitles::new(cues);
                File::create(&temp_file)?.write_all(subtitles.to_vtt().as_bytes())?;
            }
            Some(SubtitleType::Mp4Ttml) => {
                pb.write(format!(
                    " {} stpp subtitles",
                    "Extracting".colorize("bold cyan"),
                ));

                let ttml = crate::mp4parser::Mp4TtmlParser::parse_init(&subtitles_data)
                    .map_err(|x| anyhow!(x))?;
                let cues = ttml.parse_media(&subtitles_data).map_err(|x| anyhow!(x))?;
                let subtitles = crate::mp4parser::Subtitles::new(cues);
                File::create(&temp_file)?.write_all(subtitles.to_srt().as_bytes())?;
            }
            Some(SubtitleType::TtmlText) => {
                pb.write(format!(
                    " {} ttml+xml subtitles",
                    "Extracting".colorize("bold cyan"),
                ));

                let xml = String::from_utf8(subtitles_data)
                    .map_err(|_| anyhow!("cannot decode subtitles as valid utf8 string."))?;
                let ttml = crate::mp4parser::ttml_text_parser::parse(&xml).map_err(|x| {
                    anyhow!(
                        "couldn't parse xml string as ttml content (failed with {}).\n\n{}",
                        x,
                        xml
                    )
                })?;
                File::create(&temp_file)?.write_all(ttml.to_srt().as_bytes())?;
            }
            _ => File::create(&temp_file)?.write_all(&subtitles_data)?,
        };

        pb.write(format!(
            " {} stream successfully",
            "Downloaded".colorize("bold green"),
        ));
        println!(); // TODO - See Later
        pb.reset(Some(0));
    }

    // -----------------------------------------------------------------------------------------
    // Prepare Progress Bar
    // -----------------------------------------------------------------------------------------

    pb.replace(2, Column::Percentage(2));
    pb.columns
        .extend_from_slice(&[Column::text("•"), Column::text("[yellow]?")]);
    pb.pb.reset(Some(
        video_audio_streams.iter().map(|x| x.segments.len()).sum(),
    ));
    let pb = Arc::new(Mutex::new(pb));

    // -----------------------------------------------------------------------------------------
    // Estimation
    // -----------------------------------------------------------------------------------------

    let mut downloaded_bytes = 0;
    let mut relative_sizes = VecDeque::new();

    for stream in &video_audio_streams {
        if let Some(segment) = stream.segments.get(0) {
            let mut request = client.head(
                segment.seg_url(
                    baseurl
                        .as_ref()
                        .unwrap_or(&stream.uri.parse::<Url>().unwrap()),
                )?,
            );

            if let Some((range, _)) = segment.map_range(0) {
                request = request.header(RANGE, range);
            }

            let response = request.send()?;
            let content_length = response.content_length().unwrap_or(0);

            if content_length != 0 {
                relative_sizes.push_back(stream.segments.len() * (content_length as usize));
            } else {
                request = client.get(
                    segment.seg_url(
                        baseurl
                            .as_ref()
                            .unwrap_or(&stream.uri.parse::<Url>().unwrap()),
                    )?,
                );

                if let Some((range, _)) = segment.map_range(0) {
                    request = request.header(RANGE, range);
                }

                let response = request.send()?;

                relative_sizes.push_back(
                    stream.segments.len() * (response.content_length().unwrap_or(0) as usize),
                );
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // Download Video & Audio Streams
    // -----------------------------------------------------------------------------------------

    // TODO - Create a custom thread pool module.
    let pool = threadpool::ThreadPool::new(threads as usize);

    for stream in video_audio_streams {
        pb.lock().unwrap().write(format!(
            " {} {} stream {}",
            "Processing".colorize("bold green"),
            stream.media_type,
            stream.display_stream().colorize("cyan"),
        ));

        let length = stream.segments.len();

        if length == 0 {
            pb.lock().unwrap().write(format!(
                "    {} skipping stream (no segments)",
                "Warning".colorize("bold yellow"),
            ));
            continue;
        }

        let temp_file = stream
            .file_path(&directory, &stream.extension())
            .to_string_lossy()
            .to_string();
        temp_files.push(Stream {
            file_path: temp_file.clone(),
            language: stream.language.clone(),
            media_type: stream.media_type.clone(),
        });
        pb.lock().unwrap().write(format!(
            "{} stream to {}",
            "Downloading".colorize("bold green"),
            temp_file.colorize("cyan"),
        ));

        // TODO - Add resume support
        let merger = Arc::new(Mutex::new(Merger::new(stream.segments.len(), &temp_file)?));
        let timer = Arc::new(Instant::now());

        // TODO - Print this value
        let _ = relative_sizes.pop_front();
        let relative_size = relative_sizes.iter().sum();
        let mut previous_map = None;
        let mut previous_key = None;
        let mut previous_byterange_end = 0;

        for (i, segment) in stream.segments.iter().enumerate() {
            if segment.map.is_some() {
                let mut request = client.get(
                    segment
                        .map_url(
                            baseurl
                                .as_ref()
                                .unwrap_or(&stream.uri.parse::<Url>().unwrap()),
                        )?
                        .unwrap(),
                );

                if let Some((range, end)) = segment.map_range(previous_byterange_end) {
                    request = request.header(RANGE, range);
                    previous_byterange_end = end;
                }

                let response = request.send()?;
                let bytes = response.bytes()?;
                previous_map = Some(bytes.to_vec())
            }

            if !no_decrypt {
                if let Some(key) = &segment.key {
                    // TODO - Handle keyformat correctly
                    match key.method {
                        KeyMethod::Aes128 => {
                            previous_key = Some(Keys {
                                bytes: if key.key_format.is_none() {
                                    let request =
                                        client.get(
                                            segment
                                                .key_url(baseurl.as_ref().unwrap_or(
                                                    &stream.uri.parse::<Url>().unwrap(),
                                                ))?
                                                .unwrap(),
                                        );
                                    let response = request.send()?;
                                    response.bytes()?.to_vec()
                                } else {
                                    vec![]
                                },
                                iv: key.iv.clone(),
                                method: key.method.clone(),
                            });
                        }
                        KeyMethod::Cenc | KeyMethod::SampleAes => {
                            let mut decryption_keys = HashMap::new();

                            if all_keys {
                                for key in &keys {
                                    if let Some(kid) = &key.0 {
                                        decryption_keys.insert(kid.to_owned(), key.1.to_owned());
                                    }
                                }
                            } else {
                                let default_kid = stream.default_kid();

                                for key in &keys {
                                    if let Some(default_kid) = &default_kid {
                                        if let Some(kid) = &key.0 {
                                            if default_kid == kid {
                                                decryption_keys
                                                    .insert(kid.to_owned(), key.1.to_owned());
                                            }
                                        } else {
                                            decryption_keys
                                                .insert(default_kid.to_owned(), key.1.to_owned());
                                        }
                                    }
                                }
                            }

                            if decryption_keys.len() == 0 {
                                bail!(
                                    "cannot determine keys to use, bypass this error using {} flag.",
                                    "--all-keys".colorize("bold green")
                                );
                            }

                            for key in &decryption_keys {
                                pb.lock().unwrap().write(format!(
                                    "        {} {}:{}",
                                    "Key".colorize("bold green"),
                                    key.0,
                                    key.1
                                ));
                            }

                            previous_key = Some(Keys::from_hex_keys(decryption_keys));
                        }
                        _ => previous_key = None,
                    }
                }
            }

            let mut request = client.get(
                segment.seg_url(
                    baseurl
                        .as_ref()
                        .unwrap_or(&stream.uri.parse::<Url>().unwrap()),
                )?,
            );

            if let Some((range, end)) = segment.map_range(previous_byterange_end) {
                request = request.header(RANGE, range);
                previous_byterange_end = end;
            }

            let thread_data = ThreadData {
                downloaded_bytes,
                index: i,
                keys: previous_key.clone(),
                map: previous_map.clone(),
                merger: merger.clone(),
                pb: pb.clone(),
                relative_size,
                request,
                timer: timer.clone(),
                total_retries: retry_count,
            };

            if previous_key.is_none() {
                previous_map = None;
            }

            pool.execute(move || {
                if let Err(e) = thread_data.perform() {
                    let _lock = thread_data.pb.lock().unwrap();
                    println!("\n{}: {}", "error".colorize("bold red"), e);
                    std::process::exit(1);
                }
            });
        }

        pool.join();
        let mut merger = merger.lock().unwrap();
        merger.flush()?;

        if !merger.buffered() {
            bail!(
                "failed to download {} stream to {}",
                stream.display_stream().colorize("cyan"),
                temp_file
            );
        }

        downloaded_bytes += merger.stored();

        pb.lock().unwrap().write(format!(
            " {} stream successfully",
            "Downloaded".colorize("bold green"),
        ));
    }

    eprintln!();

    // -----------------------------------------------------------------------------------------
    // Mux Downloaded Streams
    // -----------------------------------------------------------------------------------------

    let video_temp_files = temp_files
        .iter()
        .filter(|x| x.media_type == MediaType::Video)
        .collect::<Vec<_>>();
    let video_streams_count = video_temp_files.len();
    let audio_streams_count = temp_files
        .iter()
        .filter(|x| x.media_type == MediaType::Audio)
        .count();
    let subtitle_streams_count = temp_files
        .iter()
        .filter(|x| x.media_type == MediaType::Subtitles)
        .count();

    if !no_decrypt
        && (video_streams_count == 1 || audio_streams_count == 1 || subtitle_streams_count == 1)
    {
        if let Some(output) = &output {
            let all_temp_files = temp_files
                .iter()
                .filter(|x| x.media_type == MediaType::Video)
                .chain(
                    temp_files
                        .iter()
                        .filter(|x| x.media_type == MediaType::Audio),
                )
                .chain(
                    temp_files
                        .iter()
                        .filter(|x| x.media_type == MediaType::Subtitles),
                )
                .collect::<Vec<_>>();

            let mut args = vec![];

            for temp_file in &all_temp_files {
                args.extend_from_slice(&["-i".to_owned(), temp_file.file_path.clone()]);
            }

            if args.len() == 2 && (video_streams_count == 1 || audio_streams_count == 1) {
                args.extend_from_slice(&["-c".to_owned(), "copy".to_owned()]);
            } else if args.len() > 2 {
                args.extend_from_slice(&["-c".to_owned(), "copy".to_owned()]);
                // args.extend_from_slice(&[
                //     "-c:v".to_owned(),
                //     "copy".to_owned(),
                //     "-c:a".to_owned(),
                //     "copy".to_owned(),
                // ]);

                if subtitle_streams_count > 0 && output.ends_with(".mp4") {
                    args.extend_from_slice(&["-c:s".to_owned(), "mov_text".to_owned()]);
                }

                for i in 0..all_temp_files.len() {
                    args.extend_from_slice(&["-map".to_owned(), i.to_string()]);
                }

                let mut audio_index = 0;
                let mut subtitle_index = 0;

                for temp_file in &all_temp_files {
                    match temp_file.media_type {
                        MediaType::Audio => {
                            if let Some(language) = &temp_file.language {
                                args.extend_from_slice(&[
                                    format!("-metadata:s:a:{}", audio_index),
                                    format!("language={}", language),
                                ]);
                            }

                            audio_index += 1;
                        }
                        MediaType::Subtitles => {
                            if let Some(language) = &temp_file.language {
                                args.extend_from_slice(&[
                                    format!("-metadata:s:s:{}", subtitle_index),
                                    format!("language={}", language),
                                ]);
                            }

                            subtitle_index += 1;
                        }
                        _ => (),
                    }
                }

                if subtitle_streams_count > 1 {
                    args.extend_from_slice(&["-disposition:s:0".to_owned(), "default".to_owned()]);
                }
            }

            args.push(output.to_owned());

            println!(
                "  {} ffmpeg {}",
                "Executing".colorize("bold cyan"),
                args.iter()
                    .map(|x| if x.contains(' ') {
                        format!("\"{}\"", x)
                    } else {
                        x.to_owned()
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            );

            if Path::new(output).exists() {
                println!("   {} {}", "Deleting".colorize("bold red"), output);
                std::fs::remove_file(output)?;
            }

            let code = Command::new("ffmpeg")
                .args(args)
                .stderr(Stdio::null())
                .spawn()?
                .wait()?;

            if !code.success() {
                bail!("ffmpeg exited with code {}", code.code().unwrap_or(1))
            }

            for temp_file in &all_temp_files {
                println!(
                    "   {} {}",
                    "Deleting".colorize("bold red"),
                    temp_file.file_path
                );
                std::fs::remove_file(&temp_file.file_path)?;
            }

            if let Some(directory) = &directory {
                if std::fs::read_dir(directory)?.next().is_none() {
                    println!(
                        "   {} {}",
                        "Deleting".colorize("bold red"),
                        directory.to_string_lossy()
                    );
                    std::fs::remove_dir(directory)?;
                }
            }
        }
    }

    Ok(())
}

struct ThreadData {
    downloaded_bytes: usize,
    index: usize,
    keys: Option<Keys>,
    map: Option<Vec<u8>>,
    merger: Arc<Mutex<Merger>>,
    pb: Arc<Mutex<RichProgress>>,
    relative_size: usize,
    request: RequestBuilder,
    timer: Arc<Instant>,
    total_retries: u8,
}

impl ThreadData {
    fn perform(&self) -> Result<()> {
        let mut segment = self.map.clone().unwrap_or(vec![]);
        segment.append(&mut self.download_segment()?);

        if let Some(keys) = &self.keys {
            segment = keys.decrypt(segment)?;
        }

        let mut merger = self.merger.lock().unwrap();
        merger.write(self.index, &segment)?;
        merger.flush()?;

        self.notify(merger.stored(), merger.estimate())?;
        Ok(())
    }

    fn download_segment(&self) -> Result<Vec<u8>> {
        let fetch_segment = || -> Result<Vec<u8>, reqwest::Error> {
            Ok(self.request.try_clone().unwrap().send()?.bytes()?.to_vec())
        };

        let mut retries = 0;
        let data = loop {
            match fetch_segment() {
                Ok(bytes) => {
                    // Download Speed
                    let elapsed_time = self.timer.elapsed().as_secs() as usize;
                    if elapsed_time != 0 {
                        let stored = self.merger.lock().unwrap().stored() + bytes.len();
                        self.pb.lock().unwrap().replace(
                            12,
                            Column::Text(format!(
                                "[yellow]{}/s",
                                utils::format_bytes(stored / elapsed_time, 2).2
                            )),
                        );
                    }

                    break bytes;
                }
                Err(e) => {
                    if self.total_retries > retries {
                        self.pb.lock().unwrap().write(check_reqwest_error(&e)?);
                        retries += 1;
                        continue;
                    } else {
                        bail!(
                            "reached maximum number of retries to download segment at index {}.",
                            self.index
                        )
                    }
                }
            }
        };

        Ok(data)
    }

    fn notify(&self, stored: usize, estimate: usize) -> Result<()> {
        let mut pb = self.pb.lock().unwrap();
        pb.replace(
            0,
            Column::Text(format!(
                "[bold blue]{}",
                utils::format_download_bytes(
                    self.downloaded_bytes + stored,
                    self.downloaded_bytes + estimate + self.relative_size,
                ),
            )),
        );
        pb.update(1);
        Ok(())
    }
}

fn check_reqwest_error(error: &reqwest::Error) -> Result<String> {
    let request = "Request".colorize("bold yellow");
    let url = error.url().unwrap();

    if error.is_timeout() {
        return Ok(format!("    {} {} (timeout)", request, url));
    } else if error.is_connect() {
        return Ok(format!("    {} {} (connection error)", request, url));
    }

    if let Some(status) = error.status() {
        match status {
            StatusCode::REQUEST_TIMEOUT => Ok(format!("    {} {} (timeout)", request, url)),
            StatusCode::TOO_MANY_REQUESTS => {
                Ok(format!("    {} {} (too many requests)", request, url))
            }
            StatusCode::SERVICE_UNAVAILABLE => {
                Ok(format!("    {} {} (service unavailable)", request, url))
            }
            StatusCode::GATEWAY_TIMEOUT => Ok(format!("    {} {} (gateway timeout)", request, url)),
            _ => bail!("download failed {} (HTTP {})", url, status),
        }
    } else {
        bail!("download failed {}", url)
    }
}