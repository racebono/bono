use crate::{
    Download,
    app::topbar::{
        audio::{SetupItem, SetupState},
        setup_label, size_text,
    },
    arrstr,
    button::ConfigButton,
    config::{ArrayStr, Config, RawConfig},
    engineer::{Heard, NotListening},
    error,
    task::{JoinHandle, MaybeDone, spawn, spawn_blocking},
    updating,
};
use camino::Utf8Path;
use egui::Widget;
use egui_extras::Column;
use bono_i18n::t;
use sdl3::{
    audio::{AudioFormat, AudioRecordingCallback, AudioSpec, AudioStream, AudioStreamWithCallback},
    event::Event,
    get_error,
    sys::{
        audio::{SDL_AudioStream, SDL_GetAudioStreamAvailable, SDL_GetAudioStreamData},
        joystick::{SDL_GetJoystickGUIDForID, SDL_JoystickID},
    },
};
use serde::Deserialize;
use std::{
    ffi::c_int,
    mem::MaybeUninit,
    num::{NonZero, NonZeroUsize},
    path::Path,
    sync::Arc,
    time::Duration,
};
use tokio::runtime::Handle;
use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState,
    get_lang_str,
};

pub const RECORDING_SPEC: AudioSpec = AudioSpec {
    freq: Some(16000),
    channels: Some(1),
    format: Some(AudioFormat::f32_sys()),
};

/// The model the setup guide downloads: it understands every language, and is quick on a GPU
const RECOMMENDED: &str = "ggml-large-v3-turbo-q5_0.bin";

pub struct Whisper {
    ctx: Option<MaybeDone<Option<WhisperContext>>>,
    /// The model `ctx` is loaded from
    loaded: Option<Arc<Utf8Path>>,
    /// The setup guide asked for the recommended model: it's downloaded, then used once it's on
    /// disk (unless another was picked meanwhile)
    setup: bool,
    /// The setup's download was started: one that fails isn't started again until it's asked
    /// for again
    setup_started: bool,
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
            loaded: None,
            setup: false,
            setup_started: false,
            stream: None,
            catalog: MaybeDone::spawn(async move {
                match Model::fetch_catalog(&pref_path, &client).await {
                    Ok(catalog) => Some(catalog),
                    Err(e) => {
                        error(t!("error.stt_catalog"), e);
                        None
                    }
                }
            }),
            threads,
        };

        if let Some(model) = config.whisper_model().clone() {
            this.set_model(model);
        }

        return this;
    }

    pub fn update(&mut self, rt: &Handle, config: &mut Config, ui: &mut egui::Ui) {
        let client = self.client.clone();
        let catalog = self.catalog_mut(rt);

        let label = setup_label(
            config.get().whisper_model().is_none(),
            t!("audio.speech_to_text"),
        );
        ui.add_enabled_ui(catalog.is_some(), move |ui| {
            ui.menu_button(label, move |ui| {
                config.maybe_update(|raw| {
                    raw.min_confidence
                        .edit(*raw.min_confidence(), |confidence| {
                            ui.add(
                                egui::Slider::new(confidence, 0.0..=1.0)
                                    .text(t!("audio.language_confidence"))
                                    .custom_formatter(|value, _| format!("{:.0}%", value * 100.0))
                                    .custom_parser(|text| {
                                        let text = text.trim().trim_end_matches('%').trim();
                                        text.parse::<f64>().ok().map(|percent| percent / 100.0)
                                    }),
                            )
                            .on_hover_text(t!("audio.language_confidence_hover"))
                            .changed()
                        })
                });
                ui.separator();

                egui_extras::TableBuilder::new(ui)
                    .id_salt("whisper-models")
                    .striped(true)
                    .columns(Column::auto(), 4)
                    .column(Column::remainder())
                    .header(20.0, |mut header| {
                        header.col(|ui| {
                            ui.strong(t!("audio.model"));
                        });
                        header.col(|ui| {
                            ui.strong(t!("audio.quantization"));
                        });
                        header.col(|ui| {
                            ui.strong(t!("audio.languages"));
                        });
                        header.col(|ui| {
                            ui.strong(t!("audio.size"));
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
                                            t!("audio.english_only")
                                        } else {
                                            t!("audio.all_languages")
                                        });
                                    });

                                    row.col(|ui| {
                                        if let Some(size) = model.size {
                                            ui.label(size_text(size));
                                        }
                                    });

                                    if let Some(on_disk) = model.on_disk() {
                                        row.col(|ui| {
                                            if on_disk {
                                                ui.add_enabled_ui(
                                                    config.whisper_model().as_ref()
                                                        != Some(&model.path),
                                                    |ui| {
                                                        if ui.button(t!("audio.use")).clicked() {
                                                            config
                                                                .whisper_model
                                                                .set(Some(model.path.clone()));
                                                            updated = true;
                                                        }
                                                    },
                                                );
                                            } else if let Some(download) = model.download() {
                                                egui::ProgressBar::new(download.progress() as f32)
                                                    .show_percentage()
                                                    .ui(ui);
                                            } else if ui
                                                .button(egui_phosphor::bold::CLOUD_ARROW_DOWN)
                                                .clicked()
                                            {
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
        device: Option<&sdl3::audio::AudioDevice>,
    ) -> Result<JoinHandle<anyhow::Result<Heard>>, bool> {
        return match (config.get().talk_button().clone(), event) {
            // The key held down repeats, but it's still the one press
            (
                Some(ConfigButton::Key(ref key_code)),
                Event::KeyDown {
                    scancode: Some(event_code),
                    repeat: false,
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
                Event::JoyButtonDown {
                    which, button_idx, ..
                },
            ) if button == button_idx
                && unsafe { SDL_GetJoystickGUIDForID(SDL_JoystickID(which.raw())) } == guid =>
            {
                self.begin(rt, device)
            }
            (
                Some(ConfigButton::Joystick { guid, ref button }),
                Event::JoyButtonUp {
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

    /// Whether the model picked is loaded, and can listen.
    pub fn ready(&mut self, rt: &Handle) -> bool {
        matches!(
            self.ctx.as_mut().and_then(|ctx| ctx.update(rt)),
            Some(Some(_))
        )
    }

    /// The microphone is open, from the talk button's press until its release.
    #[inline]
    pub fn recording(&self) -> bool {
        self.stream.is_some()
    }

    /// Opens the microphone. When it can't be, what's heard right away says why.
    fn begin(
        &mut self,
        rt: &Handle,
        device: Option<&sdl3::audio::AudioDevice>,
    ) -> Result<JoinHandle<anyhow::Result<Heard>>, bool> {
        if self.stream.is_some() {
            return Err(false);
        }

        let heard = |heard| Ok(spawn(std::future::ready(heard)));
        let not_listening = |why| heard(Ok(Heard::NotListening(why)));
        let Some(device) = device else {
            return not_listening(NotListening::NoMicrophone);
        };
        let ctx = match self.ctx.as_mut().map(|ctx| ctx.update(rt)) {
            Some(Some(Some(ctx))) => ctx,
            Some(None) => return not_listening(NotListening::Loading),
            Some(Some(None)) | None => return not_listening(NotListening::NoModel),
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

        return match f() {
            Ok(stream) => {
                self.stream = Some(stream);
                Err(true)
            }
            Err(e) => heard(Err(e.context(t!("error.recording")))),
        };
    }

    fn finish(&mut self, config: &Config) -> Result<JoinHandle<anyhow::Result<Heard>>, bool> {
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

    /// Called every frame: loads the model picked once it changes, and sets up the recommended
    /// one if the setup guide asked for it.
    pub fn poll(&mut self, rt: &Handle, config: &Config) {
        let picked = config.get().whisper_model().clone();
        if picked != self.loaded {
            match picked {
                Some(path) => self.set_model(path),
                None => {
                    self.ctx = None;
                    self.loaded = None;
                }
            }
        }

        if !self.setup {
            return;
        }
        if config.get().whisper_model().is_some() {
            self.setup = false;
            return;
        }

        let client = self.client.clone();
        let started = self.setup_started;
        let Some(model) = self.recommended(rt) else {
            return;
        };
        match model.on_disk() {
            Some(true) => {
                let path = model.path.clone();
                config.update(|raw| raw.whisper_model.set(Some(path)));
                self.setup = false;
            }
            Some(false) if !started => {
                model.start_download(&client);
                self.setup_started = true;
            }
            Some(false) => {}
            // Not looked for yet
            None => {}
        }
    }

    /// Downloads the recommended model (again, if it failed), and uses it once it's on disk, if
    /// none is picked by then.
    pub fn setup(&mut self) {
        self.setup = true;
        self.setup_started = false;
    }

    /// The recommended model, as the setup guide shows it: `None` if another is picked.
    pub fn setup_item(&mut self, rt: &Handle, config: &Config) -> Option<SetupItem> {
        let picked = config.get().whisper_model().clone();
        let setup = self.setup;
        let model = match self.catalog.update(rt) {
            None => return picked.is_none().then_some(SetupItem::Loading),
            Some(None) => return picked.is_none().then_some(SetupItem::Unavailable),
            Some(Some(catalog)) => catalog.iter().find(|model| model.file == RECOMMENDED)?,
        };
        if !setup && picked.as_ref().is_some_and(|picked| *picked != model.path) {
            return None;
        }

        let state = match (model.on_disk(), model.download()) {
            (Some(true), _) => SetupState::Ready,
            (_, Some(download)) => SetupState::Downloading(download.progress()),
            _ if model.failed() => SetupState::Failed,
            _ => SetupState::Missing,
        };
        Some(SetupItem::Found {
            name: model.name.clone(),
            size: model.size,
            state,
        })
    }

    fn recommended(&mut self, rt: &Handle) -> Option<&mut Model> {
        self.catalog_mut(rt)?
            .iter_mut()
            .find(|model| model.file == RECOMMENDED)
    }

    pub fn set_model(&mut self, path: Arc<Utf8Path>) {
        self.loaded = Some(path.clone());
        self.ctx = Some(MaybeDone::spawn_blocking(
            move || match WhisperContext::new_with_params(
                path.as_os_str(),
                WhisperContextParameters::default(),
            ) {
                Ok(ctx) => Some(ctx),
                Err(e) => {
                    error(t!("error.stt_model"), e);
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
    fn finish(self, config: Config) -> JoinHandle<anyhow::Result<Heard>> {
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
        mut stream: AudioStreamWithCallback<Recorder>,
        config: Config,
    ) -> JoinHandle<anyhow::Result<Heard>> {
        if let Err(e) = stream.pause() {
            return spawn(std::future::ready(Err(e.into())));
        }

        if let Err(e) = stream.flush() {
            return spawn(std::future::ready(Err(e.into())));
        }

        // The callback only runs when new data arrives, so read what `flush` released
        // (and anything else still queued) ourselves before the `Recorder` is dropped
        let raw = stream.raw();
        match stream.lock() {
            Some(mut recorder) => recorder.drain(raw),
            None => return spawn(std::future::ready(Err(get_error().into()))),
        }

        return spawn_blocking(move || {
            let samples = recv.recv()??;
            log::debug!("Sample count: {}", samples.len());
            if samples.len() < MIN_SAMPLES {
                return Ok(Heard::TooShort);
            }
            let (voices, min_confidence) = {
                let config = config.get();
                let voices = config.voices().keys().copied().collect::<Vec<_>>();
                (voices, *config.min_confidence())
            };

            // Nothing to answer with
            if voices.is_empty() {
                return Ok(Heard::NoVoice(None));
            }
            // The English-only model only hears English
            if !self.multilingual && !voices.contains(&arrstr!("en")) {
                return Ok(Heard::NoVoice(Some(arrstr!("en"))));
            }

            self.state.pcm_to_mel(&samples, self.threads.get())?;
            drop(samples);

            // Something said in a language without a voice is still transcribed: if it's no
            // words after all, it was noise
            return Ok(match self.detect_language(&voices, min_confidence)? {
                Spoken::Voiced(lang) => match self.speech_to_text(lang)? {
                    text if text.is_empty() => Heard::Nothing,
                    text => Heard::Question { text, lang },
                },
                Spoken::Unvoiced(lang) => match self.speech_to_text(lang)? {
                    text if text.is_empty() => Heard::Nothing,
                    text => Heard::Unvoiced { text, lang },
                },
                Spoken::Unclear => Heard::Nothing,
            });
        });
    }

    /// The language spoken, among `voices` (the languages with a voice) if it's one of them.
    fn detect_language(
        &mut self,
        voices: &[ArrayStr<2>],
        min_confidence: f32,
    ) -> anyhow::Result<Spoken> {
        if !self.multilingual {
            return Ok(Spoken::Voiced(arrstr!("en")));
        }

        // Even with a single voice: whatever was said may be in no language, or another one
        let (_, probs) = self.state.lang_detect(0, self.threads.get())?;
        let mut likeliest = (probs.iter().copied().enumerate())
            .filter_map(|(id, prob)| Some((whisper_language(id)?, prob)))
            .collect::<Vec<_>>();
        likeliest.sort_by(|(_, a), (_, b)| b.total_cmp(a));
        log::info!(
            "Languages heard on the radio: {:?}",
            &likeliest[..likeliest.len().min(3)]
        );

        return Ok(Spoken::choose(&likeliest, voices, min_confidence));
    }

    /// What was said in `lang`: empty if it's no words.
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
        // No blanks, nor tokens for sounds that aren't speech
        params.set_suppress_blank(true);
        params.set_suppress_nst(true);

        self.state.full(params, &[])?;
        let mut text = String::new();
        for segment in self.state.as_iter() {
            // Whisper makes words up for silence and noise ("Thank you.")
            let no_speech = segment.no_speech_probability();
            if no_speech > NO_SPEECH {
                log::debug!("Not speech ({no_speech}): {:?}", segment.to_str_lossy()?);
                continue;
            }
            text.push_str(segment.to_str()?);
        }

        return Ok(speech(&text));
    }
}

/// Less than this is too short to have said anything (0.3 s): the talk button was likely
/// pressed by accident.
const MIN_SAMPLES: usize = 16000 * 3 / 10;

/// How sure whisper has to be of a language without a voice to tell the pilot there's none for
/// it: noise is often a little likelier to be some language or other.
const UNVOICED_CONFIDENCE: f32 = 0.5;

/// How much less likely than one without a voice the likeliest language with a voice can be
/// before it's taken as said in the other: close languages (Catalan heard as Spanish) are
/// still answered.
const FAR_BEHIND: f32 = 0.25;

/// How likely a segment can be to be no speech at all, and still be taken as said.
const NO_SPEECH: f32 = 0.8;

/// Whisper's language `id`, as the voices have it.
fn whisper_language(id: usize) -> Option<ArrayStr<2>> {
    let code = get_lang_str(i32::try_from(id).ok()?)?;
    ArrayStr::from_str(match code {
        // Norwegian's voices are for both its written forms
        "nn" => "no",
        code => code,
    })
}

/// The language spoken on the radio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Spoken {
    /// One with a voice
    Voiced(ArrayStr<2>),
    /// One clearly without a voice
    Unvoiced(ArrayStr<2>),
    /// Unclear, if anything was said at all
    Unclear,
}

impl Spoken {
    /// The language spoken, from how likely each is (`likeliest`, likeliest first), among
    /// `voices` (the languages with a voice) if it's one of them.
    fn choose(
        likeliest: &[(ArrayStr<2>, f32)],
        voices: &[ArrayStr<2>],
        min_confidence: f32,
    ) -> Self {
        let voiced = (likeliest.iter().copied())
            .find(|(lang, prob)| voices.contains(lang) && *prob >= min_confidence);
        match (likeliest.first().copied(), voiced) {
            // Clearly another language, the likeliest with a voice far behind it
            (Some((lang, prob)), voiced)
                if !voices.contains(&lang)
                    && prob >= min_confidence.max(UNVOICED_CONFIDENCE)
                    && voiced.is_none_or(|(_, voiced)| voiced < prob * FAR_BEHIND) =>
            {
                Spoken::Unvoiced(lang)
            }
            (_, Some((lang, _))) => Spoken::Voiced(lang),
            _ => Spoken::Unclear,
        }
    }
}

/// `text` without what whisper writes for sounds that aren't speech (`[BLANK_AUDIO]`, `(music)`,
/// `♪`): empty if there are no words left.
fn speech(text: &str) -> String {
    let mut speech = String::with_capacity(text.len());
    let mut closing = None;
    for c in text.chars() {
        match (closing, c) {
            (Some(close), c) => {
                if c == close {
                    closing = None;
                }
            }
            (None, '[') => closing = Some(']'),
            (None, '(') => closing = Some(')'),
            (None, '♩' | '♪' | '♫' | '♬') => {}
            (None, c) => speech.push(c),
        }
    }

    match speech.chars().any(char::is_alphanumeric) {
        true => speech.split_whitespace().collect::<Vec<_>>().join(" "),
        false => String::new(),
    }
}

struct Segment {
    start: Duration,
    end: Duration,
    text: String,
}

struct Recorder {
    send: Option<oneshot::Sender<anyhow::Result<Vec<f32>>>>,
    samples: anyhow::Result<Vec<f32>>,
    remaining_buf: [u8; 3],
    remaining_count: usize,
}

impl Recorder {
    /// Reads everything the stream currently has available.
    fn drain(&mut self, stream: *mut SDL_AudioStream) {
        if self.send.as_ref().is_none_or(oneshot::Sender::is_closed) {
            self.send = None;
            self.samples = Ok(Vec::new());
            return;
        }

        let Ok(ref mut samples) = self.samples else {
            return;
        };

        if let Err(e) = Self::try_drain(
            stream,
            samples,
            &mut self.remaining_buf,
            &mut self.remaining_count,
        ) {
            self.samples = Err(e);
            self.send = None;
        }
    }

    fn try_drain(
        stream: *mut SDL_AudioStream,
        samples: &mut Vec<f32>,
        remaining_buf: &mut [u8; 3],
        remaining_count: &mut usize,
    ) -> anyhow::Result<()> {
        loop {
            let available = unsafe { SDL_GetAudioStreamAvailable(stream) };
            let Some(available) =
                NonZero::new(usize::try_from(available).map_err(|_| get_error())?)
            else {
                return Ok(());
            };

            if Self::try_read(stream, available, samples, remaining_buf, remaining_count)? == 0 {
                return Ok(());
            }
        }
    }

    /// Reads up to `available` bytes, returning how many were read.
    fn try_read(
        stream: *mut SDL_AudioStream,
        available: NonZero<usize>,
        samples: &mut Vec<f32>,
        remaining_buf: &mut [u8; 3],
        remaining_count: &mut usize,
    ) -> anyhow::Result<usize> {
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
                stream,
                post.as_mut_ptr().cast(),
                post.len() as c_int,
            )) {
                Ok(read) => {
                    let count = remaining_count.unchecked_add(read);
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
                    return Ok(read);
                }
                Err(_) => return Err(get_error().into()),
            }
        }
    }
}

impl AudioRecordingCallback<f32> for Recorder {
    // `_available` is the number of bytes just added in the *device's* format (divided by
    // `size_of::<f32>()` by sdl3-rs), not what can be read in `RECORDING_SPEC`, so ask SDL instead
    fn callback(&mut self, stream: &mut AudioStream, _available: i32) {
        self.drain(stream.raw());
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

    /// The download under way, or done: `None` if there's none, or it failed.
    pub fn download(&self) -> Option<&Download> {
        self.download.as_ref().filter(|download| !download.failed())
    }

    /// Whether the last download failed.
    pub fn failed(&self) -> bool {
        self.download.as_ref().is_some_and(Download::failed)
    }

    pub fn start_download(&mut self, client: &reqwest::Client) -> &mut Download {
        // One that failed starts over
        if self.failed() {
            self.download = None;
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synthesizer::{Effects, Voice};
    use camino::Utf8PathBuf;
    use std::collections::BTreeMap;
    use whisper_rs::get_lang_id;

    /// Needs `BONO_WHISPER` (a whisper model) and `BONO_VOICE_EN`/`_ES` (Piper voices).
    #[test]
    #[ignore]
    fn detects_the_language_spoken() {
        let whisper = std::env::var("BONO_WHISPER").unwrap();
        let threads = NonZeroUsize::new(4).unwrap();
        let ctx =
            WhisperContext::new_with_params(&whisper, WhisperContextParameters::default()).unwrap();

        let voices = BTreeMap::from([
            (
                arrstr!("en"),
                Utf8PathBuf::from(std::env::var("BONO_VOICE_EN").unwrap()),
            ),
            (
                arrstr!("es"),
                Utf8PathBuf::from(std::env::var("BONO_VOICE_ES").unwrap()),
            ),
        ]);
        let effects = Effects {
            radio: 0.0,
            beep_volume: 0.0,
        };

        for (lang, text) in [
            (
                arrstr!("es"),
                "¿Cuánto combustible me queda y cuándo tengo que parar?",
            ),
            (
                arrstr!("en"),
                "How much fuel do I have left, and when do I need to pit?",
            ),
        ] {
            let mut voice = Voice::load(&voices[&lang], threads).unwrap();
            let speech = voice.synthesize(text, effects).unwrap();
            // Whisper hears at 16 kHz
            let ratio = speech.sample_rate as f64 / 16000.0;
            let samples = (0..(speech.samples.len() as f64 / ratio) as usize)
                .map(|i| speech.samples[(i as f64 * ratio) as usize])
                .collect::<Vec<_>>();

            let mut session = WhisperSession {
                state: ctx.create_state().unwrap(),
                threads,
                multilingual: ctx.is_multilingual(),
            };
            session.state.pcm_to_mel(&samples, threads.get()).unwrap();
            let (_, probs) = session.state.lang_detect(0, threads.get()).unwrap();
            for code in ["en", "es"] {
                println!("{code}: {}", probs[get_lang_id(code).unwrap() as usize]);
            }

            let candidates = voices.keys().copied().collect::<Vec<_>>();
            let Spoken::Voiced(detected) = session.detect_language(&candidates, 0.1).unwrap()
            else {
                panic!("{lang} wasn't detected among the voices");
            };
            let heard = session.speech_to_text(detected).unwrap();
            println!("{lang} -> {detected}: {heard}");
            assert_eq!(detected, lang);
        }
    }

    #[test]
    fn chooses_the_language_spoken() {
        let [en, es, fr, ca, pt] = [
            arrstr!("en"),
            arrstr!("es"),
            arrstr!("fr"),
            arrstr!("ca"),
            arrstr!("pt"),
        ];
        let both = [en, es];
        let choose = |likeliest: &[(ArrayStr<2>, f32)], voices: &[ArrayStr<2>]| {
            Spoken::choose(likeliest, voices, 0.1)
        };

        assert_eq!(choose(&[(es, 0.8), (en, 0.1)], &both), Spoken::Voiced(es));
        // Another language, with no voice for it
        assert_eq!(
            choose(&[(fr, 0.9), (en, 0.05)], &both),
            Spoken::Unvoiced(fr)
        );
        // A close one is answered in the voice nearest it
        assert_eq!(choose(&[(ca, 0.6), (es, 0.3)], &both), Spoken::Voiced(es));
        // Far behind it, even if it's likely enough
        assert_eq!(
            choose(&[(fr, 0.85), (en, 0.12)], &both),
            Spoken::Unvoiced(fr)
        );
        // Noise isn't any language in particular
        assert_eq!(choose(&[(fr, 0.3), (en, 0.05)], &both), Spoken::Unclear);
        assert_eq!(choose(&[], &both), Spoken::Unclear);
        // With a single voice, too
        assert_eq!(
            choose(&[(en, 0.9), (es, 0.05)], &[es]),
            Spoken::Unvoiced(en)
        );
        assert_eq!(choose(&[(es, 0.4), (pt, 0.3)], &[es]), Spoken::Voiced(es));
        assert_eq!(choose(&[(en, 0.2), (es, 0.05)], &[es]), Spoken::Unclear);
    }

    #[test]
    fn leaves_out_what_isnt_speech() {
        assert_eq!(speech(" How are the tyres? "), "How are the tyres?");
        assert_eq!(speech("[BLANK_AUDIO]"), "");
        assert_eq!(speech(" (music) ♪ "), "");
        assert_eq!(speech(" ... "), "");
        assert_eq!(speech("[engine noise] Box  this lap?"), "Box this lap?");
        assert_eq!(
            speech("¿Cuánto (risas) combustible?"),
            "¿Cuánto combustible?"
        );
    }
}
