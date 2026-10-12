use crate::{
    Download, arrstr,
    button::ConfigButton,
    config::{ArrayStr, Config, RawConfig},
    error,
    task::{JoinHandle, MaybeDone, spawn, spawn_blocking},
    updating,
};
use anyhow::Context;
use camino::{Utf8Path, Utf8PathBuf};
use egui::Widget;
use egui_extras::Column;
use sdl3::{
    audio::{AudioFormat, AudioRecordingCallback, AudioSpec, AudioStream, AudioStreamWithCallback},
    event::Event,
    get_error,
    sys::{
        audio::SDL_GetAudioStreamData,
        joystick::{SDL_GetJoystickGUIDForID, SDL_JoystickID},
    },
};
use serde::Deserialize;
use std::{
    collections::btree_map,
    ffi::c_int,
    mem::MaybeUninit,
    num::{NonZero, NonZeroUsize},
    path::Path,
    sync::Arc,
    time::Duration,
};
use tokio::runtime::Handle;
use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperError,
    WhisperState, get_lang_id,
};

pub const RECORDING_SPEC: AudioSpec = AudioSpec {
    freq: Some(16000),
    channels: Some(1),
    format: Some(AudioFormat::f32_sys()),
};

pub struct Whisper {
    ctx: Option<MaybeDone<Option<WhisperContext>>>,
    stream: Option<WhisperStream>,
    catalog: MaybeDone<Option<Vec<Model>>>,
    threads: NonZeroUsize,
    client: reqwest::Client,
}

impl Whisper {
    pub fn new(
        pref_path: &Arc<Utf8Path>,
        config: &RawConfig,
        client: reqwest::Client,
        threads: Option<NonZeroUsize>,
    ) -> Self {
        let pref_path = pref_path.clone();
        let threads = threads
            .or_else(|| std::thread::available_parallelism().ok())
            .unwrap_or(NonZeroUsize::MIN);

        let mut this = Self {
            client: client.clone(),
            ctx: None,
            stream: None,
            catalog: MaybeDone::spawn(async move {
                match Model::fetch_catalog(&pref_path, &client).await {
                    Ok(catalog) => Some(catalog),
                    Err(e) => {
                        error("Error fetching speech to text catalog", e);
                        None
                    }
                }
            }),
            threads,
        };

        if let Some(model) = config.whisper_model.clone() {
            this.set_model(model);
        }

        return this;
    }

    pub fn update(&mut self, rt: &Handle, config: &mut Config, ui: &mut egui::Ui) {
        let client = self.client.clone();
        let catalog = self.catalog_mut(rt);

        ui.add_enabled_ui(catalog.is_some(), move |ui| {
            ui.menu_button("Speech To Text", move |ui| {
                egui_extras::TableBuilder::new(ui)
                    .id_salt("whisper-models")
                    .striped(true)
                    .columns(Column::auto(), 3)
                    .column(Column::remainder())
                    .header(20.0, |mut header| {
                        header.col(|ui| {
                            ui.strong("Model");
                        });
                        header.col(|ui| {
                            ui.strong("Quantization");
                        });
                        header.col(|ui| {
                            ui.strong("Languages");
                        });
                    })
                    .body(|mut body| {
                        config.maybe_update(|config| {
                            let mut updated = false;
                            for model in catalog.unwrap_or_default() {
                                body.row(18.0, |mut row| {
                                    row.col(|ui| {
                                        ui.label(&model.name);
                                    });

                                    row.col(|ui| {
                                        if let Some(ref q) = model.quantization {
                                            ui.label(q);
                                        }
                                    });

                                    row.col(|ui| {
                                        ui.label(if model.english_only {
                                            "English"
                                        } else {
                                            "All"
                                        });
                                    });

                                    if let Some(on_disk) = model.on_disk() {
                                        row.col(|ui| {
                                            if on_disk {
                                                ui.add_enabled_ui(
                                                    config.whisper_model.as_ref()
                                                        != Some(&model.path),
                                                    |ui| {
                                                        if ui.button("Use").clicked() {
                                                            config.whisper_model =
                                                                Some(model.path.clone());
                                                            updated = true;
                                                        }
                                                    },
                                                );
                                            } else if let Some(download) = model.download() {
                                                egui::ProgressBar::new(download.progress() as f32)
                                                    .show_percentage()
                                                    .ui(ui);
                                            } else if ui.button("Download").clicked() {
                                                model.start_download(&client);
                                            }
                                        });
                                    }
                                });
                            }
                            return updated;
                        });
                    });
            });
        });
    }

    pub fn event(
        &mut self,
        rt: &Handle,
        event: &Event,
        config: &Config,
        device: &sdl3::audio::AudioDevice,
    ) -> Result<JoinHandle<anyhow::Result<Option<(String, ArrayStr<2>)>>>, bool> {
        return match (config.get().talk_button.clone(), event) {
            (
                Some(ConfigButton::Key(ref key_code)),
                Event::KeyDown {
                    scancode: Some(event_code),
                    ..
                },
            ) if key_code == event_code => self.begin(rt, device),
            (
                Some(ConfigButton::Key(ref key_code)),
                Event::KeyUp {
                    scancode: Some(event_code),
                    ..
                },
            ) if key_code == event_code => self.finish(config),
            (
                Some(ConfigButton::Joystick { guid, ref button }),
                Event::JoyButtonUp {
                    which, button_idx, ..
                },
            ) if button == button_idx
                && unsafe { SDL_GetJoystickGUIDForID(SDL_JoystickID(which.raw())) } == guid =>
            {
                self.begin(rt, device)
            }
            (
                Some(ConfigButton::Joystick { guid, ref button }),
                Event::JoyButtonDown {
                    which, button_idx, ..
                },
            ) if button == button_idx
                && unsafe { SDL_GetJoystickGUIDForID(SDL_JoystickID(which.raw())) } == guid =>
            {
                self.finish(config)
            }
            _ => Err(false),
        };
    }

    fn begin(
        &mut self,
        rt: &Handle,
        device: &sdl3::audio::AudioDevice,
    ) -> Result<JoinHandle<anyhow::Result<Option<(String, ArrayStr<2>)>>>, bool> {
        let Some(ref mut ctx) = self.ctx else {
            return Err(false);
        };

        if self.stream.is_some() {
            return Err(false);
        }

        let ctx = match ctx.update(rt) {
            Some(Some(ctx)) => ctx,
            Some(None) | None => return Err(false),
        };

        let f = || {
            let (send, recv) = oneshot::channel();
            let stream = device.open_recording_stream_with_callback(
                &RECORDING_SPEC,
                Recorder {
                    send: Some(send),
                    samples: Ok(Vec::new()),
                    remaining_buf: [0; 3],
                    remaining_count: 0,
                },
            )?;
            stream.resume()?;
            log::debug!("Started microphone recording");

            return Ok::<_, anyhow::Error>(WhisperStream {
                recv,
                session: WhisperSession {
                    state: ctx.create_state()?,
                    threads: self.threads,
                    multilingual: ctx.is_multilingual(),
                },
                stream,
            });
        };

        self.stream = match f() {
            Ok(stream) => Some(stream),
            Err(e) => {
                error("Error begining microphone recording", e);
                None
            }
        };

        return Err(true);
    }

    fn finish(
        &mut self,
        config: &Config,
    ) -> Result<JoinHandle<anyhow::Result<Option<(String, ArrayStr<2>)>>>, bool> {
        let Some(stream) = self.stream.take() else {
            return Err(false);
        };
        log::debug!("Finishing microphone recording");
        return Ok(stream.finish(config.clone()));
    }
}

impl Whisper {
    pub fn catalog(&mut self, rt: &Handle) -> Option<&[Model]> {
        match self.catalog.update(rt) {
            Some(Some(catalog)) => Some(catalog),
            Some(None) | None => None,
        }
    }

    fn catalog_mut(&mut self, rt: &Handle) -> Option<&mut [Model]> {
        match self.catalog.update(rt) {
            Some(Some(catalog)) => Some(catalog),
            Some(None) | None => None,
        }
    }

    pub fn set_model(&mut self, path: Arc<Utf8Path>) {
        self.ctx = Some(MaybeDone::spawn_blocking(
            move || match WhisperContext::new_with_params(
                path.as_os_str(),
                WhisperContextParameters::default(),
            ) {
                Ok(ctx) => Some(ctx),
                Err(e) => {
                    error("Error loading speech to text model", e);
                    None
                }
            },
        ));
    }
}

struct WhisperStream {
    recv: oneshot::Receiver<anyhow::Result<Vec<f32>>>,
    session: WhisperSession,
    stream: AudioStreamWithCallback<Recorder>,
}

impl WhisperStream {
    fn finish(self, config: Config) -> JoinHandle<anyhow::Result<Option<(String, ArrayStr<2>)>>> {
        return self.session.finish(self.recv, self.stream, config);
    }
}

pub struct WhisperSession {
    state: WhisperState,
    threads: NonZeroUsize,
    multilingual: bool,
}

impl WhisperSession {
    fn finish(
        mut self,
        recv: oneshot::Receiver<anyhow::Result<Vec<f32>>>,
        stream: AudioStreamWithCallback<Recorder>,
        config: Config,
    ) -> JoinHandle<anyhow::Result<Option<(String, ArrayStr<2>)>>> {
        if let Err(e) = stream.pause() {
            return spawn(std::future::ready(Err(e.into())));
        }

        if let Err(e) = stream.flush() {
            return spawn(std::future::ready(Err(e.into())));
        }

        return spawn_blocking(move || {
            let samples = recv.recv()??;
            let config = config.get();

            self.state.pcm_to_mel(&samples, self.threads.get())?;
            drop(samples);

            if let Some(lang) = self.detect_language(config.voices.keys(), config.min_confidence)? {
                drop(config);
                return Ok(Some((self.speech_to_text(lang)?, lang)));
            } else {
                return Ok(None);
            }
        });
    }

    fn detect_language(
        &mut self,
        mut candidates: btree_map::Keys<'_, ArrayStr<2>, Utf8PathBuf>,
        min_confidence: f32,
    ) -> anyhow::Result<Option<ArrayStr<2>>> {
        return Ok(Some(arrstr!("en")));

        if candidates.len() == 1 {
            return Ok(Some(
                *candidates.next().context("Expected a language candidate")?,
            ));
        }

        if !self.multilingual {
            if !candidates.any(|k| k == &arrstr!("en")) {
                return Ok(None);
            }
            return Ok(Some(arrstr!("en")));
        }

        let (_, probs) = self.state.lang_detect(0, self.threads.get())?;
        let mut chosen = None::<(ArrayStr<2>, f32)>;
        for candidate in candidates {
            let Some(id) = get_lang_id(candidate) else {
                log::warn!("Unsupported language ({candidate})");
                continue;
            };

            let Some(prob) = usize::try_from(id).ok().and_then(|i| probs.get(i)).copied() else {
                continue;
            };

            if prob < min_confidence {
                continue;
            }

            if chosen
                .as_ref()
                .is_none_or(|(_, cand_prob)| prob > *cand_prob)
            {
                chosen = Some((*candidate, prob));
            }
        }

        return Ok(chosen.map(|(name, _)| name));
    }

    fn speech_to_text(&mut self, lang: ArrayStr<2>) -> anyhow::Result<String> {
        let mut params = FullParams::new(SamplingStrategy::BeamSearch {
            beam_size: 5,
            patience: -1.0,
        });

        params.set_language(Some(&lang));
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);

        self.state.full(params, &[])?;
        let text = self
            .state
            .as_iter()
            .map(|x| x.to_str())
            .collect::<Result<String, WhisperError>>()?;

        return Ok(trim_in_place(text));
    }
}

struct Segment {
    start: Duration,
    end: Duration,
    text: String,
}

fn trim_in_place(s: String) -> String {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    let offset = unsafe { trimmed.as_ptr().offset_from_unsigned(s.as_ptr()) };
    let len = trimmed.len();

    let mut bytes = s.into_bytes();
    drop(bytes.drain(..offset));
    unsafe { bytes.set_len(len) };
    return unsafe { String::from_utf8_unchecked(bytes) };
}

struct Recorder {
    send: Option<oneshot::Sender<anyhow::Result<Vec<f32>>>>,
    samples: anyhow::Result<Vec<f32>>,
    remaining_buf: [u8; 3],
    remaining_count: usize,
}

impl Recorder {
    fn try_callback(
        stream: &mut AudioStream,
        available: i32,
        samples: &mut Vec<f32>,
        remaining_buf: &mut [u8; 3],
        remaining_count: &mut usize,
    ) -> anyhow::Result<()> {
        let Some(available) = NonZero::new(available.max(0) as usize) else {
            todo!()
        };

        unsafe {
            samples.try_reserve(
                available
                    .get()
                    .div_ceil(4)
                    .unchecked_add((*remaining_count > 0) as usize),
            )?;

            let spare_bytes = std::slice::from_raw_parts_mut(
                samples
                    .spare_capacity_mut()
                    .as_mut_ptr()
                    .cast::<MaybeUninit<u8>>(),
                available.get().unchecked_add(*remaining_count),
            );

            let (pre, post) = spare_bytes.split_at_mut_unchecked(*remaining_count);
            pre.write_copy_of_slice(remaining_buf.get_unchecked(..*remaining_count));

            match usize::try_from(SDL_GetAudioStreamData(
                stream.raw(),
                post.as_mut_ptr().cast(),
                post.len() as c_int,
            )) {
                Ok(count) => {
                    let count = remaining_count.unchecked_add(count);
                    let (full, partial) = spare_bytes
                        .get_unchecked(..count)
                        .assume_init_ref()
                        .as_chunks::<4>();

                    let len = samples.len();
                    samples.set_len(len.unchecked_add(full.len()));

                    std::ptr::copy_nonoverlapping(
                        partial.as_ptr(),
                        remaining_buf.as_mut_ptr(),
                        partial.len(),
                    );
                    *remaining_count = partial.len();
                }
                Err(_) => return Err(get_error().into()),
            }

            return Ok(());
        }
    }
}

impl AudioRecordingCallback<f32> for Recorder {
    fn callback(&mut self, stream: &mut AudioStream, available: i32) {
        if self.send.as_ref().is_none_or(oneshot::Sender::is_closed) {
            self.send = None;
            self.samples = Ok(Vec::new());
            return;
        }

        let Ok(ref mut samples) = self.samples else {
            return;
        };

        if let Err(e) = Self::try_callback(
            stream,
            available,
            samples,
            &mut self.remaining_buf,
            &mut self.remaining_count,
        ) {
            self.samples = Err(e);
            self.send = None;
        }
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        let Some(send) = self.send.take() else { return };

        if self.remaining_count != 0 {
            log::warn!("Audio recording returned unexpected trailing bytes");
        }

        let _ = send.send(std::mem::replace(&mut self.samples, Ok(Vec::new())));
    }
}

const CATALOG_URL: &str = "https://huggingface.co/api/models/ggerganov/whisper.cpp/tree/main";
const FILE_URL: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/";

pub struct Model {
    pub size: Option<u64>,
    pub name: String,
    pub english_only: bool,
    pub quantization: Option<String>,
    pub sha256: Option<String>,
    pub file: String,
    pub path: Arc<Utf8Path>,
    on_disk: tokio::sync::watch::Receiver<Option<bool>>,
    download: Option<Download>,
}

impl Model {
    #[inline]
    pub fn on_disk(&self) -> Option<bool> {
        *self.on_disk.borrow()
    }

    pub fn download(&self) -> Option<&Download> {
        self.download.as_ref()
    }

    pub fn start_download(&mut self, client: &reqwest::Client) -> &mut Download {
        self.download.get_or_insert_with(|| {
            let path = Arc::into_raw(self.path.clone());
            let path = unsafe { Arc::<Path>::from_raw(path as *const Path) };
            Download::new(
                client.get(format!("{FILE_URL}{}", self.file)),
                path,
                self.size,
            )
        })
    }

    async fn fetch_catalog(
        pref_dir: &Utf8Path,
        client: &reqwest::Client,
    ) -> reqwest::Result<Vec<Model>> {
        #[derive(Debug, Clone, Deserialize)]
        struct Lfs {
            #[serde(default)]
            size: Option<u64>,
            #[serde(default)]
            oid: Option<String>,
        }

        #[derive(Debug, Clone, Deserialize)]
        struct Catalog {
            #[serde(default)]
            r#type: Option<String>,
            #[serde(default)]
            size: Option<u64>,
            #[serde(default)]
            oid: Option<String>,
            #[serde(default)]
            lfs: Option<Lfs>,
            #[serde(default)]
            path: Option<String>,
        }

        let catalog = client
            .get(CATALOG_URL)
            .send()
            .await?
            .error_for_status()?
            .json::<Vec<Catalog>>()
            .await?;

        let mut models = Vec::<Model>::new();
        for entry in catalog {
            if entry.r#type.as_deref() != Some("file") {
                continue;
            }

            let Some(file) = entry.path else {
                continue;
            };

            let Some(name) = file
                .strip_prefix("ggml-")
                .and_then(|file| file.strip_suffix(".bin"))
            else {
                continue;
            };

            if name.contains('/') {
                continue;
            }

            let (name, quantization) = match name.rsplit_once("-q") {
                Some((name, quantization)) => (name, Some(quantization)),
                None => (name, None),
            };

            let (name, english_only) = match name.strip_suffix(".en") {
                Some(name) => (name, true),
                None => (name, false),
            };

            let size = match (entry.size, entry.lfs.as_ref()) {
                (.., Some(lfs)) => lfs.size,
                (size @ Some(..), None) => size,
                (None, None) => None,
            };

            let oid = match (entry.oid, entry.lfs) {
                (.., Some(lfs)) => lfs.oid,
                (oid @ Some(..), None) => oid,
                (None, None) => None,
            };

            let path = Arc::<Utf8Path>::from(pref_dir.join("whisper").join(&file));
            models.push(Model {
                size,
                name: name.to_owned(),
                english_only,
                quantization: quantization.map(ToOwned::to_owned),
                sha256: oid,
                file,
                path: path.clone(),
                on_disk: updating(Duration::from_millis(1234), None, move |_| {
                    let path = path.clone();
                    async move { tokio::fs::try_exists(&*path).await.ok() }
                }),
                download: None,
            });
        }

        models.sort_by_key(|lhs| lhs.size);
        return Ok(models);
    }
}
