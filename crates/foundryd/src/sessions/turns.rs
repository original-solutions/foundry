//! Each turn's final message on disk, and `session_turn`.
//!
//! For a factory session, a `Stop` writes `turns/<n>.txt` in the session's folder and only then
//! the session's `session_turn` record with turn `n`, so a reader who sees the number finds the
//! file. A `UserPromptSubmit` adds one to the prompt count and takes the question away; a
//! `Notification` that makes the session wait writes `question.txt`. A session keeps the counts
//! it `want`s in memory, and the counts its record holds as `stored`; both are read from the
//! store when the daemon has not seen the session since it started.
//!
//! A hook is applied in two steps. [`Inner::prepare`] does what must happen once, writes the
//! file and changes `want`, and a failure of it changes nothing, so it can be run again.
//! [`Inner::flush`] can run any number of times: it applies the state and writes the record
//! when `want` differs from `stored`. The sweeper flushes a session whose record is still
//! behind, so a record that failed past the hook's retries is stored later.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use foundry_proto::factory::{FactoryBody, FactoryRecord, SessionTurn};
use foundry_proto::ids::{ProjectId, SessionId};
use foundry_proto::record::Session;
use foundry_proto::state::SessionState;

use super::state::next_state;
use super::state::{Event, Hook};
use super::{Inner, Live, SessionError, Sessions, blocking, claude, factory, lock};
use crate::secrets::Secrets;

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
    /// The newest question, asked since the last prompt: only a prompt clears it.
    pub question: Option<String>,
}

/// The numbers a `session_turn` record holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Counts {
    /// The newest turn's number.
    pub turn: u64,
    /// How many prompts were submitted.
    pub prompts: u64,
    /// Whether a question was asked since the last prompt. Only a prompt clears it.
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

/// What a session's record holds, and what it should hold once its hooks are flushed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Tally {
    /// The counts after every hook prepared so far.
    pub want: Counts,
    /// The counts of the newest record written.
    pub stored: Counts,
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

/// Hides secrets in what is kept on disk: the gateway key and the live sessions' tokens, then
/// what [`crate::secrets::hide_known`] hides. It reads the secret store, so it runs on the
/// blocking pool.
pub(super) struct Hider {
    memory: Vec<String>,
    secrets: Option<Arc<dyn Secrets>>,
}

impl Hider {
    /// Hides `secret` too, a token that is not yet in the live sessions.
    pub fn also(mut self, secret: String) -> Self {
        self.memory.push(secret);
        self
    }

    /// `text` as it is kept on disk. With no secret store, none of it is kept.
    pub fn hide(&self, text: &str) -> String {
        let Some(secrets) = &self.secrets else {
            return crate::secrets::HIDDEN_UNREADABLE.to_owned();
        };
        let known: Vec<&str> = self.memory.iter().map(String::as_str).collect();
        crate::secrets::hide_known(&hide_each(text, &known), secrets.as_ref())
    }
}

impl Sessions {
    /// Turn `turn`'s final message of factory session `session`, and the question that waits.
    /// `None` when this daemon knows no such factory session, or when a turn from 1 up has no
    /// readable file, as when its folder is gone. Turn 0 has an empty message.
    pub async fn turn_text(&self, session: SessionId, turn: u64) -> Option<TurnText> {
        lock(&self.inner.factory).marks.get(&session)?;
        let folder = factory::folder(&self.inner.dir, session);
        blocking(move || {
            let message = if turn == 0 {
                String::new()
            } else {
                std::fs::read_to_string(turn_file(&folder, turn)).ok()?
            };
            let question = std::fs::read_to_string(question_file(&folder)).ok();
            Some(TurnText { message, question })
        })
        .await
        .ok()
        .flatten()
    }
}

impl Inner {
    /// The secrets this daemon holds in memory: every live session's hook token and the
    /// gateway key. [`crate::secrets::hide_known`] does not know them.
    pub(super) fn memory_secrets(&self) -> Vec<String> {
        let mut secrets: Vec<String> = lock(&self.live)
            .values()
            .map(|live| live.token.clone())
            .collect();
        secrets.push(self.config.gateway.key.clone());
        secrets
    }

    /// What hides secrets in what is written to disk, as of now.
    pub(super) fn hider(&self) -> Hider {
        Hider {
            memory: self.memory_secrets(),
            secrets: self.secrets.get().cloned(),
        }
    }

    /// Applies `hook` to a factory session, with `text`, which is `last_assistant_message` of
    /// a `Stop` and `message` of a `Notification`, and empty for any other hook. `prepared`
    /// says whether [`Inner::prepare`] already succeeded for this hook, and is set when it
    /// does, so a caller that tries again does not count the hook twice. A session that is not
    /// a live factory session just takes the event.
    pub(super) async fn hook_step(
        &self,
        live: &Arc<Live>,
        hook: Hook,
        text: &str,
        prepared: &mut bool,
    ) -> Result<SessionState, SessionError> {
        let mut body = live.body.lock().await;
        let project = lock(&self.factory).marks.get(&live.id).copied();
        let Some(project) = project.filter(|_| self.is_live(live)) else {
            return self.event_locked(live, &mut body, Event::Hook(hook)).await;
        };
        if !*prepared {
            self.prepare(&body, hook, text).await?;
            *prepared = true;
        }
        let state = self
            .event_locked(live, &mut body, Event::Hook(hook))
            .await?;
        self.flush(&body, project).await?;
        Ok(state)
    }

    /// The once-only part of a hook, with the session's lock held: the file, and then `want`.
    /// A failure changes nothing in memory.
    async fn prepare(&self, body: &Session, hook: Hook, text: &str) -> Result<(), SessionError> {
        let id = body.id;
        let folder = factory::folder(&self.dir, id);
        match hook {
            Hook::Stop => {
                let next = self.tally(id).await?.want.after_stop();
                self.write_file(turn_file(&folder, next.turn), text).await?;
                self.change(id, |want| *want = want.after_stop());
            }
            Hook::UserPromptSubmit => {
                self.tally(id).await?;
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
                self.change(id, |want| *want = want.after_prompt());
            }
            Hook::Notification => {
                // Claude Code also notifies after a minute idle: that asks nothing.
                let asks = body.state != SessionState::WaitingForYou
                    && next_state(body.state, Event::Hook(hook)) == SessionState::WaitingForYou;
                if asks {
                    self.tally(id).await?;
                    self.write_file(question_file(&folder), text).await?;
                    self.change(id, |want| *want = want.after_question());
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Changes the counts the session wants. Its tally is in memory: `prepare` loaded it.
    fn change(&self, session: SessionId, change: impl FnOnce(&mut Counts)) {
        if let Some(tally) = lock(&self.factory).counts.get_mut(&session) {
            change(&mut tally.want);
        }
    }

    /// The session's tally: kept in memory, or else read from the record in the store, or else
    /// zeros.
    pub(super) async fn tally(&self, session: SessionId) -> Result<Tally, SessionError> {
        if let Some(tally) = lock(&self.factory).counts.get(&session) {
            return Ok(*tally);
        }
        let found = self
            .on_store(move |store| store.session_turn(session))
            .await?;
        let stored = found.map_or_else(Counts::default, |turn| Counts {
            turn: turn.turn,
            prompts: turn.prompts,
            question: turn.question,
        });
        let tally = Tally {
            want: stored,
            stored,
        };
        Ok(*lock(&self.factory).counts.entry(session).or_insert(tally))
    }

    /// Writes the session's record when the counts it wants are not the ones it holds, and
    /// keeps them as stored once it is written. It can be run again at any time. The caller
    /// holds the session's lock.
    pub(super) async fn flush(
        &self,
        session: &Session,
        project: ProjectId,
    ) -> Result<(), SessionError> {
        let tally = self.tally(session.id).await?;
        if tally.want == tally.stored {
            return Ok(());
        }
        #[cfg(test)]
        if super::take_one(&self.fail_turn_writes) {
            return Err(super::busy().into());
        }
        let me = self.me;
        let want = tally.want;
        let turn = SessionTurn {
            session: session.id,
            workspace: session.workspace,
            project,
            turn: want.turn,
            prompts: want.prompts,
            question: want.question,
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
        if let Some(tally) = lock(&self.factory).counts.get_mut(&session.id) {
            tally.stored = want;
        }
        Ok(())
    }

    /// Writes `text` to `path` as [`write_text`] does, hidden as [`Hider`] hides it.
    async fn write_file(&self, path: PathBuf, text: &str) -> Result<(), SessionError> {
        let (hider, text) = (self.hider(), text.to_owned());
        blocking(move || write_text(&path, &hider.hide(&text))).await?
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
