//! Each turn's final message on disk, and `session_turn`.
//!
//! For a factory session, a `Stop` writes `turns/<n>.txt` in the session's folder and only then
//! the session's `session_turn` record with turn `n`, so a reader who sees the number finds the
//! file. A `UserPromptSubmit` adds one to the prompt count and takes the question away; a
//! `Notification` that makes the session wait writes `question.txt`. The counts are kept in
//! memory once the record that holds them is stored, and read from the store when the daemon
//! has not seen the session since it started.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use foundry_proto::factory::{FactoryBody, FactoryRecord, SessionTurn};
use foundry_proto::ids::{ProjectId, SessionId};
use foundry_proto::record::Session;
use foundry_proto::state::SessionState;

use super::state::{Event, Hook};
use super::{Inner, Live, SessionError, Sessions, blocking, claude, factory, lock};

/// The folder in a session's folder that holds a file per turn.
const TURNS: &str = "turns";

/// The file in a session's folder that holds the newest question.
const QUESTION: &str = "question.txt";

/// What replaces a secret in what is kept on disk.
const HIDDEN: &str = "***";

/// A turn's final message and the question that waits, as [`Sessions::turn_text`] gives them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnText {
    /// The turn's whole final message, with known secrets hidden. Empty for turn 0.
    pub message: String,
    /// The newest question, while one waits.
    pub question: Option<String>,
}

/// The numbers a `session_turn` record holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Counts {
    /// The newest turn's number.
    pub turn: u64,
    /// How many prompts were submitted.
    pub prompts: u64,
    /// Whether a question waits.
    pub question: bool,
}

impl Counts {
    /// The counts after a turn ends.
    pub fn after_stop(self) -> Counts {
        Counts {
            turn: self.turn + 1,
            ..self
        }
    }

    /// The counts after a prompt is submitted: one more, and no question waits.
    pub fn after_prompt(self) -> Counts {
        Counts {
            prompts: self.prompts + 1,
            question: false,
            ..self
        }
    }

    /// The counts after the session asked a question.
    pub fn after_question(self) -> Counts {
        Counts {
            question: true,
            ..self
        }
    }
}

/// The file that holds turn `turn`'s message, in the session's `folder`.
pub(super) fn turn_file(folder: &Path, turn: u64) -> PathBuf {
    folder.join(TURNS).join(format!("{turn}.txt"))
}

/// The file that holds the newest question, in the session's `folder`.
pub(super) fn question_file(folder: &Path) -> PathBuf {
    folder.join(QUESTION)
}

/// `text` with each non-empty one of `secrets` hidden.
pub(super) fn hide_each(text: &str, secrets: &[&str]) -> String {
    secrets
        .iter()
        .filter(|secret| !secret.is_empty())
        .fold(text.to_owned(), |text, secret| text.replace(secret, HIDDEN))
}

/// Writes `text` to `path`, whole or not at all: to `<path>.new` first, readable by this user
/// only, in a folder that is too, and then over `path`. A `.new` file left by an earlier try is
/// removed first.
pub(super) fn write_text(path: &Path, text: &str) -> Result<(), SessionError> {
    let mut name = path.as_os_str().to_owned();
    name.push(".new");
    let new = PathBuf::from(name);
    match std::fs::remove_file(&new) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(SessionError::Io {
                what: format!("{}: remove the old file", new.display()),
                source,
            });
        }
    }
    claude::write_new_private(&new, text.as_bytes())?;
    std::fs::rename(&new, path).map_err(|source| SessionError::Io {
        what: format!("{}: put the file in place", path.display()),
        source,
    })
}

impl Sessions {
    /// Turn `turn`'s final message of factory session `session`, and the question that waits.
    /// `None` when this daemon knows no such factory session, or when a turn from 1 up has no
    /// readable file, as when its folder is gone. Turn 0 has an empty message.
    pub fn turn_text(&self, session: SessionId, turn: u64) -> Option<TurnText> {
        lock(&self.inner.factory).marks.get(&session)?;
        let folder = factory::folder(&self.inner.dir, session);
        let message = if turn == 0 {
            String::new()
        } else {
            std::fs::read_to_string(turn_file(&folder, turn)).ok()?
        };
        let question = std::fs::read_to_string(question_file(&folder)).ok();
        Some(TurnText { message, question })
    }
}

impl Inner {
    /// Applies `hook` to the session, with `text`, which is `last_assistant_message` of a
    /// `Stop` and `message` of a `Notification`, and empty for any other hook. For a factory
    /// session the turn files and the `session_turn` record are written too, under the same
    /// hold of the session's lock as the state. Any other hook, and every hook of a P1
    /// session, is [`Inner::event`].
    pub(super) async fn hook_event(
        &self,
        live: &Arc<Live>,
        hook: Hook,
        text: &str,
    ) -> Result<SessionState, SessionError> {
        let mut body = live.body.lock().await;
        let project = lock(&self.factory).marks.get(&live.id).copied();
        let Some(project) = project.filter(|_| self.is_live(live)) else {
            return self.event_locked(live, &mut body, Event::Hook(hook)).await;
        };
        let event = Event::Hook(hook);
        let folder = factory::folder(&self.dir, live.id);
        match hook {
            Hook::Stop => {
                let next = self.counts(live.id).await?.after_stop();
                self.write_file(turn_file(&folder, next.turn), text).await?;
                let state = self.event_locked(live, &mut body, event).await?;
                self.store_counts(&body, project, next).await?;
                Ok(state)
            }
            Hook::UserPromptSubmit => {
                let counts = self.counts(live.id).await?;
                let state = self.event_locked(live, &mut body, event).await?;
                let question = question_file(&folder);
                blocking(move || match std::fs::remove_file(&question) {
                    Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
                    _ => Ok(()),
                })
                .await?
                .map_err(|source| SessionError::Io {
                    what: "the question file: remove it".into(),
                    source,
                })?;
                self.store_counts(&body, project, counts.after_prompt())
                    .await?;
                Ok(state)
            }
            // Claude Code also notifies after a minute idle: that asks nothing.
            Hook::Notification
                if matches!(
                    body.state,
                    SessionState::Starting | SessionState::Working | SessionState::Stuck
                ) =>
            {
                let counts = self.counts(live.id).await?;
                self.write_file(question_file(&folder), text).await?;
                let state = self.event_locked(live, &mut body, event).await?;
                self.store_counts(&body, project, counts.after_question())
                    .await?;
                Ok(state)
            }
            _ => self.event_locked(live, &mut body, event).await,
        }
    }

    /// The session's counts: those kept in memory, or else the record in the store, or else
    /// zeros.
    pub(super) async fn counts(&self, session: SessionId) -> Result<Counts, SessionError> {
        if let Some(counts) = lock(&self.factory).counts.get(&session) {
            return Ok(*counts);
        }
        let found = self
            .on_store(move |store| store.session_turn(session))
            .await?;
        let counts = found.map_or_else(Counts::default, |turn| Counts {
            turn: turn.turn,
            prompts: turn.prompts,
            question: turn.question,
        });
        Ok(*lock(&self.factory).counts.entry(session).or_insert(counts))
    }

    /// Writes `counts` as the session's `session_turn` record, and keeps them in memory once it
    /// is stored.
    async fn store_counts(
        &self,
        session: &Session,
        project: ProjectId,
        counts: Counts,
    ) -> Result<(), SessionError> {
        let me = self.me;
        let turn = SessionTurn {
            session: session.id,
            workspace: session.workspace,
            project,
            turn: counts.turn,
            prompts: counts.prompts,
            question: counts.question,
        };
        self.on_store(move |store| {
            let record = FactoryRecord {
                owner: me,
                version: store.own_next_factory_version()?,
                body: FactoryBody::Turn(turn),
            };
            store.apply_factory(&record, me)?;
            Ok(())
        })
        .await?;
        lock(&self.factory).counts.insert(session.id, counts);
        Ok(())
    }

    /// Writes `text` to `path` as [`write_text`] does, with what [`Inner::hide_for_disk`]
    /// hides.
    async fn write_file(&self, path: PathBuf, text: &str) -> Result<(), SessionError> {
        let shown = self.hide_for_disk(text);
        blocking(move || write_text(&path, &shown)).await?
    }

    /// `text` as it is kept on disk: the gateway key and every live session's token hidden, and
    /// then what [`crate::secrets::hide_known`] hides. With no secret store, none of it is kept.
    fn hide_for_disk(&self, text: &str) -> String {
        let Some(secrets) = self.secrets.get() else {
            return crate::secrets::HIDDEN_UNREADABLE.to_owned();
        };
        let tokens: Vec<String> = lock(&self.live)
            .values()
            .map(|live| live.token.clone())
            .collect();
        let mut known: Vec<&str> = tokens.iter().map(String::as_str).collect();
        known.push(&self.config.gateway.key);
        crate::secrets::hide_known(&hide_each(text, &known), secrets.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    #[test]
    fn a_stop_raises_turn_and_keeps_the_rest() {
        let before = Counts {
            turn: 4,
            prompts: 9,
            question: true,
        };
        assert_eq!(
            before.after_stop(),
            Counts {
                turn: 5,
                prompts: 9,
                question: true
            }
        );
    }

    #[test]
    fn a_prompt_adds_one_and_clears_the_question() {
        let before = Counts {
            turn: 4,
            prompts: 9,
            question: true,
        };
        assert_eq!(
            before.after_prompt(),
            Counts {
                turn: 4,
                prompts: 10,
                question: false
            }
        );
    }

    #[test]
    fn a_question_sets_the_flag_and_keeps_the_counts() {
        let before = Counts {
            turn: 4,
            prompts: 9,
            question: false,
        };
        assert_eq!(
            before.after_question(),
            Counts {
                turn: 4,
                prompts: 9,
                question: true
            }
        );
    }

    #[test]
    fn hide_each_replaces_every_secret_and_skips_empty_ones() {
        let text = "key K1 and K2 and K1 again";
        assert_eq!(
            hide_each(text, &["K1", "", "K2"]),
            "key *** and *** and *** again"
        );
        assert_eq!(hide_each(text, &[]), text);
    }

    #[test]
    fn a_text_file_is_replaced_whole_and_private_and_a_stale_new_file_is_no_matter() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("turns").join("1.txt");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path.with_extension("txt.new"), "stale").unwrap();

        write_text(&path, "first").unwrap();
        write_text(&path, "second").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(!path.with_extension("txt.new").exists());
    }

    #[test]
    fn a_turn_and_the_question_have_their_own_files_in_the_session_folder() {
        let folder = std::path::Path::new("/data/sessions/s");
        assert_eq!(turn_file(folder, 3), folder.join("turns").join("3.txt"));
        assert_eq!(question_file(folder), folder.join("question.txt"));
    }
}
