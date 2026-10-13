//! The race engineer: what the pilot says on the radio (transcribed) goes to Claude, which answers
//! from the running simulator's telemetry with the `engineer` tool, and the answer is spoken back
//! over the radio.

use crate::{
    arrstr,
    claude::{
        Conversation, ConversationBuilder, Reply, StopReason,
        engineer::{self, Engineer as EngineerTool},
    },
    config::{ArrayStr, Config},
    sim::Sim,
    sqlite::pool::Pool,
    synthesizer::{Speech, Synthesizer},
    task::Task,
};
use sdl3::audio::{AudioDevice, AudioStreamOwner};
use std::{path::PathBuf, sync::Arc};
use tokio::{
    runtime::Handle,
    sync::{Mutex, oneshot},
};

/// Said when there's no telemetry to answer from.
const NO_TELEMETRY: &str = "No telemetry yet, get in the car.";
/// Said when Claude declines to answer.
const SAY_AGAIN: &str = "Say again?";

/// One line of the conversation, as the chat shows it.
pub struct ChatEntry {
    pub speaker: Speaker,
    pub text: String,
    /// Small print under it (e.g. the tokens an answer took)
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speaker {
    Pilot,
    Engineer,
    /// The app: sessions starting, errors, cancelled questions
    Notice,
}

pub struct Engineer {
    client: reqwest::Client,
    /// Everything said, for the chat
    chat: Vec<ChatEntry>,
    /// The language the pilot last spoke on the radio: typed questions are answered in it
    last_lang: ArrayStr<2>,
    /// The conversation for the database the tool reads. `None` until the first question, and
    /// again once the simulator stops, letting go of its database
    session: Option<Session>,
    /// A new session was asked for: the next question starts one
    new_session: bool,
    /// The question being answered, and the language to answer in
    turn: Option<(Task<anyhow::Result<Reply>>, ArrayStr<2>)>,
    /// The answer being synthesized
    speech: Option<oneshot::Receiver<anyhow::Result<Speech>>>,
    /// The answer being played: dropping it stops it
    playback: Option<AudioStreamOwner>,
}

struct Session {
    /// The database the tool reads
    db: PathBuf,
    state: Arc<Mutex<SessionState>>,
}

struct SessionState {
    conversation: Conversation,
    pool: Pool,
    /// The game session the conversation is about (track, session type and number), for
    /// `auto_session`
    game_session: Option<String>,
}

impl Engineer {
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            client,
            chat: Vec::new(),
            last_lang: arrstr!("en"),
            session: None,
            new_session: false,
            turn: None,
            speech: None,
            playback: None,
        }
    }

    /// Everything said so far, oldest first.
    #[inline]
    pub fn chat(&self) -> &[ChatEntry] {
        &self.chat
    }

    /// Whether a question is being answered.
    #[inline]
    pub fn is_thinking(&self) -> bool {
        self.turn.is_some() || self.speech.is_some()
    }

    /// Starts a new conversation with the next question: the engineer forgets what was said.
    pub fn new_session(&mut self) {
        self.stop();
        self.new_session = true;
        self.notice("New session: the engineer starts over");
    }

    /// A question typed in the chat, answered like one asked on the radio (spoken in the
    /// language last used on the radio).
    pub fn ask_text(
        &mut self,
        rt: &Handle,
        sim: Option<&mut Sim>,
        config: &Config,
        synth: &mut Synthesizer,
        question: String,
    ) {
        let lang = self.last_lang;
        self.ask(rt, sim, config, synth, question, lang);
    }

    /// The pilot asked something on the radio: it's sent to Claude, with the running simulator's
    /// telemetry. An answer still on its way, or being spoken, is dropped: the pilot talked over
    /// it.
    pub fn ask(
        &mut self,
        rt: &Handle,
        sim: Option<&mut Sim>,
        config: &Config,
        synth: &mut Synthesizer,
        question: String,
        lang: ArrayStr<2>,
    ) {
        self.stop();
        self.last_lang = lang;
        self.chat.push(ChatEntry {
            speaker: Speaker::Pilot,
            text: question.clone(),
            detail: None,
        });

        let Some(pool) = sim.and_then(|sim| sim.database()).cloned() else {
            self.reply(NO_TELEMETRY.into(), Some("no telemetry".into()));
            self.say(rt, config, synth, NO_TELEMETRY.into(), arrstr!("en"));
            return;
        };

        let db = pool.database().path().to_owned();
        if self.new_session || self.session.as_ref().is_none_or(|s| s.db != db) {
            match self.start(config, pool) {
                Ok(session) => self.session = Some(session),
                Err(e) => {
                    self.notice(format!("Error starting the race engineer: {e:#}"));
                    return;
                }
            }
            self.new_session = false;
        }

        let Some(session) = &self.session else {
            return;
        };
        let state = session.state.clone();
        let auto_session = config.get().auto_session;
        let turn = Task::spawn(async move {
            let mut state = state.lock().await;
            if auto_session {
                state.follow_game_session().await;
            }
            state.conversation.send(question).await
        });
        self.turn = Some((turn, lang));
    }

    /// Called every frame: answers come in, are synthesized and played. `sim_running`: whether a
    /// simulator is running at all.
    pub fn update(
        &mut self,
        rt: &Handle,
        sim_running: bool,
        config: &Config,
        synth: &mut Synthesizer,
        speaker: Option<&AudioDevice>,
    ) {
        // The game stopped: the conversation lets go of the database, so the plugin can delete it
        if !sim_running && self.session.is_some() {
            self.turn = None;
            self.session = None;
            self.notice("The simulator stopped: the session ended");
        }

        if let Some((turn, lang)) = &mut self.turn
            && let Some(result) = turn.update(rt)
        {
            let lang = *lang;
            self.turn = None;
            match result {
                Ok(reply) => self.answer(rt, config, synth, reply, lang),
                Err(e) => self.notice(format!("Error asking the race engineer: {e:#}")),
            }
        }

        if let Some(speech) = &mut self.speech {
            match speech.try_recv() {
                Ok(Ok(mut speech)) => {
                    self.speech = None;
                    let gain = {
                        let config = config.get();
                        config.master_volume * config.engineer_volume
                    };
                    for sample in &mut speech.samples {
                        *sample *= gain;
                    }

                    match speaker {
                        Some(speaker) => match speech.play(speaker) {
                            Ok(stream) => self.playback = Some(stream),
                            Err(e) => self.notice(format!("Error playing the answer: {e}")),
                        },
                        None => self.notice("No speaker selected: the answer isn't played"),
                    }
                }
                Ok(Err(e)) => {
                    self.speech = None;
                    self.notice(format!("Error synthesizing the answer: {e:#}"));
                }
                Err(oneshot::error::TryRecvError::Empty) => {}
                Err(oneshot::error::TryRecvError::Closed) => self.speech = None,
            }
        }

        // Once SDL has played all of it, the stream can go
        if let Some(stream) = &self.playback
            && stream.queued_bytes().is_ok_and(|queued| queued == 0)
        {
            self.playback = None;
        }
    }

    fn answer(&mut self, rt: &Handle, config: &Config, synth: &mut Synthesizer, reply: Reply, lang: ArrayStr<2>) {
        log::info!(
            "Engineer ({}, {:?}; {} in, {} cached, {} out): {}",
            reply.model,
            reply.stop,
            reply.usage.input_tokens + reply.usage.cache_creation_input_tokens,
            reply.usage.cache_read_input_tokens,
            reply.usage.output_tokens,
            reply.text
        );

        let text = match reply.stop {
            // The exchange was forgotten: the pilot can ask again
            StopReason::Refusal => SAY_AGAIN.to_owned(),
            // Every later question would end the same way: the next one starts over
            StopReason::ContextWindowExceeded => {
                self.new_session = true;
                reply.text
            }
            StopReason::EndTurn | StopReason::StopSequence | StopReason::MaxTokens => reply.text,
        };

        let usage = &reply.usage;
        let mut detail = format!(
            "{} | {} in, {} cached, {} out",
            reply.model,
            usage.input_tokens + usage.cache_creation_input_tokens,
            usage.cache_read_input_tokens,
            usage.output_tokens
        );
        match reply.stop {
            StopReason::Refusal => detail.push_str(" | declined"),
            StopReason::MaxTokens => detail.push_str(" | cut off"),
            StopReason::ContextWindowExceeded => detail.push_str(" | conversation full, starting over"),
            StopReason::EndTurn | StopReason::StopSequence => {}
        }

        if !text.trim().is_empty() {
            self.reply(text.clone(), Some(detail));
            self.say(rt, config, synth, text, lang);
        }
    }

    /// Speaks `text` in `lang`, or in English without a voice for it.
    fn say(&mut self, rt: &Handle, config: &Config, synth: &mut Synthesizer, text: String, lang: ArrayStr<2>) {
        let (lang, effects) = {
            let config = config.get();
            let lang = match config.voices().contains_key(&lang) {
                true => lang,
                false => arrstr!("en"),
            };
            (lang, config.synth_effects())
        };
        self.speech = Some(synth.synthesize(rt, lang, text, effects));
    }

    /// Drops the answer on its way and stops the one being spoken.
    fn stop(&mut self) {
        if self.turn.take().is_some() {
            self.notice("Cancelled: the pilot talked over it");
        }
        self.speech = None;
        self.playback = None;
    }

    fn reply(&mut self, text: String, detail: Option<String>) {
        self.chat.push(ChatEntry {
            speaker: Speaker::Engineer,
            text,
            detail,
        });
    }

    fn notice(&mut self, text: impl Into<String>) {
        let text = text.into();
        log::info!("{text}");
        self.chat.push(ChatEntry {
            speaker: Speaker::Notice,
            text,
            detail: None,
        });
    }

    fn start(&self, config: &Config, pool: Pool) -> anyhow::Result<Session> {
        let tool = EngineerTool::new(pool.clone());
        let mut builder = ConversationBuilder::new();
        {
            let config = config.get();
            builder
                .model(&config.claude_model)
                .effort(&config.claude_effort)
                .system(engineer::instructions())
                .tool(
                    EngineerTool::NAME,
                    Some(EngineerTool::description()),
                    EngineerTool::input_schema(),
                    tool.clone(),
                );
        }
        let conversation = builder.build(self.client.clone())?;

        // The history cache is built now, so the first history question doesn't wait for it
        tokio::spawn(async move {
            if let Err(e) = tool.warm().await {
                log::debug!("Not warming the engineer's history yet: {e:#}");
            }
        });

        let db = pool.database().path().to_owned();
        log::info!("Engineer session started on {}", db.display());
        Ok(Session {
            db,
            state: Arc::new(Mutex::new(SessionState {
                conversation,
                pool,
                game_session: None,
            })),
        })
    }
}

impl SessionState {
    /// A new game session (another track, or the next practice, qualifying or race) starts the
    /// conversation over.
    async fn follow_game_session(&mut self) {
        let current = match game_session(&self.pool).await {
            Ok(current) => current,
            Err(e) => {
                log::warn!("Error reading the game session: {e:#}");
                return;
            }
        };

        if current.is_some() && current != self.game_session {
            if self.game_session.is_some() {
                log::info!("New game session: the engineer starts over");
                self.conversation.reset();
            }
            self.game_session = current;
        }
    }
}

/// The game session in the database: its track, type and number.
async fn game_session(pool: &Pool) -> anyhow::Result<Option<String>> {
    let conn = pool.get().await?;
    Ok(conn
        .run(|conn| {
            use rusqlite::OptionalExtension;
            conn.query_row(
                "SELECT track, session_type, session_number FROM session ORDER BY id DESC LIMIT 1",
                [],
                |row| {
                    Ok(format!(
                        "{}|{}|{}",
                        row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                        row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                        row.get::<_, Option<i64>>(2)?.unwrap_or_default()
                    ))
                },
            )
            .optional()
        })
        .await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlite::Database;
    use lambiase_sim::{
        recorder::{DATABASE, Recorder},
        sys::lambiase_session_info,
    };
    use std::num::NonZeroUsize;

    fn session(kind: &str, number: i64) -> lambiase_session_info {
        let mut session = lambiase_session_info::default();
        session.track = Some("Circuit de la Sarthe".to_owned()).into();
        session.session_type = Some(kind.to_owned()).into();
        session.session_number = Some(number).into();
        session
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn follows_the_game_session() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = Recorder::new(dir.path(), None).unwrap();
        recorder.push_scoring(session("practice", 1), Vec::new());
        // Written by the recorder's thread, soon
        let path = dir.path().join(DATABASE);
        let one = NonZeroUsize::new(1).unwrap();
        let pool = Pool::new(Database::read_only(Arc::new(path)), one, one);

        let mut builder = ConversationBuilder::new();
        builder.api_key("test");
        let mut state = SessionState {
            conversation: builder.build(reqwest::Client::new()).unwrap(),
            pool,
            game_session: None,
        };

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while state.game_session.is_none() {
            assert!(std::time::Instant::now() < deadline, "no session recorded");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            state.follow_game_session().await;
        }
        assert_eq!(state.game_session.as_deref(), Some("Circuit de la Sarthe|practice|1"));

        recorder.push_scoring(session("race", 1), Vec::new());
        drop(recorder);
        state.follow_game_session().await;
        assert_eq!(state.game_session.as_deref(), Some("Circuit de la Sarthe|race|1"));
    }
}
