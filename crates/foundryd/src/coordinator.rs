//! The priority order and the shared settings sequence.
//!
//! [`acting`] says which machine holds the coordinator role: the first machine in the priority
//! order that this machine can reach. Only that machine numbers edits to the shared settings
//! ([`numbered`]); every other machine forwards an edit to it. Copies of the settings meet
//! through [`merge`], one setting at a time, so a machine never loses an edit it has not
//! passed on yet, and every machine reaches the same result whatever order copies arrive in.
//!
//! "Later" is the higher number, never wall time: of two edits to one setting the higher
//! stamp holds ([`Stamp`]'s order). Two stamps conflict only when neither copy has seen the
//! other's, which is two sides of a split each editing the setting. A conflict leaves a
//! "setting overwritten" item, itself a setting, holding both values.
//!
//! An edit is sent under an [`EditId`] its sender picks, and the acting machine answers it with
//! an [`Outcome`]: numbered, with its stamp, or refused. The sender keeps the edit until the
//! answer, or a copy of the settings that holds the id's stamp ([`remember`]), arrives, and
//! sends it again when the link or the role moves. An id is numbered once by any machine that
//! holds the copy that numbered it, so its sender learns what became of the edit; two sides of
//! a split that cannot reach each other can still number it once each.
//!
//! A machine reports an edit refused, or failed with a store error, only when no machine can
//! have numbered its id. Otherwise the outcome is the stamp, or [`CoordinatorError::NoAnswer`]
//! when [`KEEP_FORWARDED`] passes without it. So a sender's refusal is final only when the edit
//! went to one machine, whose own copy holds every id it numbered; once an edit went to two, a
//! refusal from either is ignored, as the other may have numbered it.
//!
//! [`Coordinator`] runs this on a machine. Record links hand it every copy of the settings a
//! peer sends and every edit forwarded to this machine ([`crate::sync::Peers::coordinate`]).
//! A machine that comes to act numbers nothing until it has taken a copy from every other
//! machine it can reach, except one linked only in a protocol that carries no settings, so a
//! returning machine catches up before it edits; an edit that reaches it before then waits,
//! at most [`OFFLINE_AFTER`].

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use foundry_proto::ids::MachineId;
use foundry_proto::messages::ANSWERS_SINCE;
use foundry_proto::settings::{
    Edit, EditId, Outcome, Overwritten, SettingKey, SettingValue, SharedSettings, Stamp, StampKey,
};
use serde::Serialize;
use tokio::sync::oneshot;
use tokio::time::Instant;

use crate::store::{Store, StoreError};
use crate::sync::{OFFLINE_AFTER, Peers, Unsent, lock, on_store};

/// How often an edit that waits looks again.
const STEP: Duration = Duration::from_millis(100);

/// How long an edit sent to the acting machine waits for an answer or its stamp, from when it
/// was made or arrived. It is also how long an edit a peer forwarded is kept, from when it
/// arrived, while this machine cannot send it to the machine it takes to act: no link to it is
/// open, or its links speak a protocol that does not carry the edit. A machine silent for
/// [`OFFLINE_AFTER`] is no longer reachable, so by twice that the role has moved to one this
/// machine can reach, unless the acting machine is reachable and older.
const KEEP_FORWARDED: Duration = OFFLINE_AFTER.saturating_mul(2);

/// How far below the newest numbered edit an edit's id is kept in [`SharedSettings::numbered`].
/// It must cover every edit the acting machine can number within [`KEEP_FORWARDED`], the longest
/// a sender sends one again.
pub const NUMBERED_KEPT: u64 = 128;

/// Why an edit was not made.
#[derive(Debug, thiserror::Error)]
pub enum CoordinatorError {
    #[error("no link to {0}, the acting coordinator, is open, so the edit was not sent")]
    NotLinked(MachineId),
    #[error(
        "{acting}, the acting coordinator, speaks protocol {speaks}, which does not answer \
         edits, so the edit was not sent"
    )]
    Older { acting: MachineId, speaks: u16 },
    #[error("the edit was refused: {reason}")]
    Refused { reason: String },
    #[error(
        "no answer came from {acting}, the acting coordinator, within {KEEP_FORWARDED:?}; the \
         edit may or may not have been numbered"
    )]
    NoAnswer { acting: MachineId },
    #[error("the edit cannot be written, so it was not sent: {0}")]
    Unwritable(serde_json::Error),
    #[error(
        "an edit {from} forwarded was dropped: {KEEP_FORWARDED:?} after it arrived this \
         machine still cannot send it to {acting}, which acts as it sees it"
    )]
    Unsettled { from: MachineId, acting: MachineId },
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// This machine's part in the coordinator role: it numbers edits while it acts, forwards them
/// while another machine does, and merges every copy of the settings a peer sends.
pub struct Coordinator {
    me: MachineId,
    store: Arc<Mutex<Store>>,
    peers: Peers,
    /// The machines this one has taken a copy of the settings from, and when it last took one.
    /// Each link sends its copy before anything else, so a machine is in here by the time it
    /// is heard from. Written and read for a waiting edit under the store's lock, with the
    /// settings, so a copy counted here is always in the settings read with it.
    taken: Arc<Mutex<BTreeMap<MachineId, Instant>>>,
    /// When this machine started. Until every listed machine's copy is in, or
    /// [`OFFLINE_AFTER`] has passed, its own copy may be an old one.
    started: Instant,
    /// Released when the edit forwarded last is numbered here or first sent on, or its task
    /// ends. Each forwarded edit waits for the one before, so they are numbered and first sent
    /// in the order they arrived.
    last_forwarded: Mutex<Option<oneshot::Receiver<()>>>,
    /// The ids of the forwarded edits with a task queued or running, so an edit sent again
    /// while its first copy waits is not made twice.
    forwarding: Mutex<BTreeSet<(MachineId, EditId)>>,
    /// The ids this machine answered refused, with the refusal and when, kept for
    /// [`KEEP_FORWARDED`]: a refusal is final for its id. A timeout, not an order of records, so
    /// the clock is fine here.
    refused: Mutex<BTreeMap<EditId, (Instant, Outcome)>>,
    /// The ids of the edits this machine waits on, how many wait on each, and the answer to the
    /// id once it came.
    answers: Mutex<BTreeMap<EditId, (usize, Option<Outcome>)>>,
}

impl Coordinator {
    pub fn new(me: MachineId, store: Arc<Mutex<Store>>, peers: Peers) -> Self {
        Self {
            me,
            store,
            peers,
            taken: Arc::new(Mutex::new(BTreeMap::new())),
            started: Instant::now(),
            last_forwarded: Mutex::new(None),
            forwarding: Mutex::new(BTreeSet::new()),
            refused: Mutex::new(BTreeMap::new()),
            answers: Mutex::new(BTreeMap::new()),
        }
    }

    /// The machine that holds the role, as this machine sees it now.
    pub async fn acting(&self) -> Result<MachineId, StoreError> {
        let held = self.held().await?;
        Ok(acting(&held.priority, &self.peers.online()))
    }

    /// Makes `edit` and returns its stamp. On the acting machine it is numbered once this
    /// machine has caught up, or after [`OFFLINE_AFTER`]. On any other machine it is sent to the
    /// acting one under a new [`EditId`], and this waits for the answer or for a copy of the
    /// settings that holds the stamp, sending it again to whichever machine acts when the link
    /// closes or the role moves. It fails with the refusal's reason when the one machine it was
    /// sent to refuses it; with [`CoordinatorError::NoAnswer`] when [`KEEP_FORWARDED`] passes
    /// with neither an answer nor the stamp, whatever else failed since it was sent, or when it
    /// went to two machines and one refused; and with an error that says nothing was sent when
    /// no link to the acting machine is open or it speaks a protocol that does not answer
    /// edits, when the edit cannot be written, or when the store fails before any send.
    pub async fn edit(&self, edit: Edit) -> Result<Stamp, CoordinatorError> {
        self.edit_from(None, Instant::now(), EditId::random(), edit)
            .await
    }

    /// Takes the answer to the edit sent under `id`, when this machine waits on it. An answer
    /// nobody waits for is dropped.
    pub(crate) fn take_answer(&self, id: EditId, outcome: Outcome) {
        if let Some((_, slot)) = lock(&self.answers).get_mut(&id) {
            // A stamp from any machine settles the edit, so it replaces a refusal.
            if slot.is_none() || matches!(outcome, Outcome::Numbered { .. }) {
                *slot = Some(outcome);
            }
        }
    }

    /// Merges a copy of the settings `peer` sent into the held copy.
    ///
    /// A copy that came on a link in a protocol before [`ANSWERS_SINCE`] cannot carry the ids
    /// of numbered edits, so it is taken as holding the ones this copy holds: without that every
    /// merge with it would differ from both copies and raise the number again.
    pub(crate) async fn take_settings(
        &self,
        peer: MachineId,
        speak: u16,
        mut settings: SharedSettings,
    ) -> Result<(), StoreError> {
        let (taken, at) = (Arc::clone(&self.taken), Instant::now());
        on_store(&self.store, move |store| {
            let held = store.settings()?;
            if speak < ANSWERS_SINCE {
                settings.numbered = held.numbered.clone();
            }
            let merged = merge(&held, &settings);
            if merged != held {
                store.apply_settings(merged)?;
            }
            lock(&taken).insert(peer, at);
            Ok(())
        })
        .await
    }

    /// Makes an edit `peer` forwarded, in the background, so the link that carried it goes on
    /// reading while the edit waits. Forwarded edits are numbered and first sent one at a time,
    /// in the order they arrived, and each one's waits count from when it arrived, not from its
    /// turn. When the edit came with an `id`, `peer` is answered under it: numbered, or refused
    /// with a fixed reason that names no setting value. A refusal is final for its id: an edit
    /// sent again under it is refused again, for [`KEEP_FORWARDED`]. An edit sent again by the
    /// same machine while its first copy is queued or running is dropped: that copy answers. An
    /// edit from a machine before [`ANSWERS_SINCE`] has no id: it is passed on as it came, or
    /// numbered here, and never answered.
    pub(crate) fn take_edit(self: Arc<Self>, peer: MachineId, id: Option<EditId>, edit: Edit) {
        let arrived = Instant::now();
        let queued = match id {
            Some(id) => match Queued::new(&self, peer, id) {
                Some(queued) => Some(queued),
                None => return,
            },
            None => None,
        };
        let coordinator = Arc::clone(&self);
        let (released, next) = oneshot::channel();
        let before = lock(&self.last_forwarded).replace(next);
        tokio::spawn(async move {
            let _queued = queued;
            if let Some(before) = before {
                // Resolves when the edit before is numbered or first sent, or its task ends.
                let _ = before.await;
            }
            let Some(id) = id else {
                if let Err(error) = coordinator.pass_on(peer, arrived, edit, released).await {
                    tracing::warn!("coordinator: {error}");
                }
                return;
            };
            let mut released = Some(released);
            let outcome = match coordinator.refusal(id) {
                Some(refused) => Some(refused),
                None => {
                    let result = coordinator
                        .run_edit(Some(peer), arrived, id, edit, &mut released)
                        .await;
                    if let Err(error) = &result {
                        tracing::warn!("coordinator: {error}");
                    }
                    coordinator.outcome(result)
                }
            };
            if let Some(outcome) = outcome {
                if matches!(outcome, Outcome::Refused { .. }) {
                    lock(&coordinator.refused).insert(id, (Instant::now(), outcome.clone()));
                }
                // Only now may the next edit go on: a copy of this id sees the refusal.
                drop(released);
                coordinator.peers.send_answer(peer, id, outcome).await;
            }
        });
    }

    /// The refusal this machine gave `id` within [`KEEP_FORWARDED`], if it did.
    fn refusal(&self, id: EditId) -> Option<Outcome> {
        let mut refused = lock(&self.refused);
        refused.retain(|_, (at, _)| at.elapsed() < KEEP_FORWARDED);
        refused.get(&id).map(|(_, outcome)| outcome.clone())
    }

    /// What to tell a machine whose forwarded edit ended in `result`; `None` when nothing is
    /// told. A reason is a fixed phrase built from machine ids only: never a store error's text
    /// or a setting value.
    fn outcome(&self, result: Result<Stamp, CoordinatorError>) -> Option<Outcome> {
        let me = self.me;
        let reason = match result {
            Ok(stamp) => return Some(Outcome::Numbered { stamp }),
            Err(CoordinatorError::Refused { reason }) => reason,
            Err(CoordinatorError::Store(_)) => format!("{me} could not store it"),
            Err(CoordinatorError::Unwritable(_)) => {
                format!("{me} could not write it to pass it on")
            }
            Err(CoordinatorError::Unsettled { acting, .. }) => {
                format!("{me} could not send it to {acting}, the acting coordinator")
            }
            // Whether it was numbered is unknown: the forwarder's own limit reports it. The
            // rest cannot happen for a forwarded edit.
            Err(
                CoordinatorError::NoAnswer { .. }
                | CoordinatorError::NotLinked(_)
                | CoordinatorError::Older { .. },
            ) => return None,
        };
        Some(Outcome::Refused { reason })
    }

    /// Makes `edit`, sent under `id`, which `from` forwarded, or this machine's own when `None`;
    /// `arrived` is when it reached this machine. See [`Coordinator::run_edit`].
    async fn edit_from(
        &self,
        from: Option<MachineId>,
        arrived: Instant,
        id: EditId,
        edit: Edit,
    ) -> Result<Stamp, CoordinatorError> {
        self.run_edit(from, arrived, id, edit, &mut None).await
    }

    /// Makes `edit`, sent under `id`, which `from` forwarded, or this machine's own when `None`;
    /// `arrived` is when it reached this machine. Only the acting machine numbers it, once
    /// caught up or [`OFFLINE_AFTER`] after it arrived, and it numbers an `id` once. `released`
    /// is fired once the edit is first sent.
    ///
    /// Every [`STEP`] it looks again. An answer to `id` settles it, and so does a copy of the
    /// settings that holds a stamp for `id`. Otherwise, while another machine acts, the edit is
    /// sent to it, and sent again when the link it went on closes or another machine acts
    /// ([`Coordinator::look`] says where it goes).
    ///
    /// It reports a refusal, or a store error, only when no machine can have numbered `id`:
    /// a refusal ends the wait only while the edit went to at most one machine, whose own copy
    /// holds every id it numbered, and a store error ends it only before the edit was ever
    /// sent. Once it was sent, a refusal from one of two machines and any store error are
    /// logged and it waits on, and [`KEEP_FORWARDED`] after it arrived it ends as
    /// [`CoordinatorError::NoAnswer`], naming the machine it went to last.
    async fn run_edit(
        &self,
        from: Option<MachineId>,
        arrived: Instant,
        id: EditId,
        edit: Edit,
        released: &mut Option<oneshot::Sender<()>>,
    ) -> Result<Stamp, CoordinatorError> {
        let _waiting = self.wait_on(id);
        // The acting machine this edit was last sent to, and the number of the link it went on.
        let mut sent: Option<(MachineId, u64)> = None;
        // Every machine it was sent to.
        let mut sent_to: BTreeSet<MachineId> = BTreeSet::new();
        // The last store error, and whether a refusal was ignored, logged once each.
        let mut logged: Option<String> = None;
        let mut ignored = false;
        loop {
            let answered = lock(&self.answers)
                .get(&id)
                .and_then(|(_, outcome)| outcome.clone());
            match answered {
                Some(Outcome::Numbered { stamp }) => return Ok(stamp),
                Some(Outcome::Refused { reason }) if sent_to.len() <= 1 => {
                    return Err(CoordinatorError::Refused { reason });
                }
                Some(Outcome::Refused { .. }) if !ignored => {
                    ignored = true;
                    tracing::warn!(
                        "coordinator: a refusal of an edit sent to two machines; waiting"
                    );
                }
                _ => {}
            }
            // The error to report when the limit passes with nothing settled.
            let mut failed: Option<StoreError> = None;
            match self.look(from, arrived, &mut logged).await {
                // A read that fails ends only this machine's own edit that was never sent.
                Err(error) if from.is_none() && sent.is_none() => return Err(error.into()),
                Err(error) => failed = Some(error),
                Ok(look) => {
                    if let Some(stamp) = look.held.numbered.get(&id) {
                        return Ok(*stamp);
                    }
                    match look.next {
                        // A write that fails ends an edit that was never sent, forwarded or
                        // not. The numbering machine cannot know another did not number it, so
                        // it does not try: the sender's rule above stops a second machine's
                        // refusal being taken as final.
                        Next::Number => match self.number(Some(id), &edit).await {
                            Ok(stamp) => return Ok(stamp),
                            Err(CoordinatorError::Store(error)) if sent.is_none() => {
                                return Err(error.into());
                            }
                            Err(CoordinatorError::Store(error)) => failed = Some(error),
                            Err(error) => return Err(error),
                        },
                        Next::Send(acting) => {
                            let on_its_way = sent.is_some_and(|(to, link)| {
                                to == acting && self.peers.is_open(acting, link)
                            });
                            if !on_its_way {
                                match self.peers.send_edit(acting, Some(id), &edit).await {
                                    Ok(link) => {
                                        sent = Some((acting, link));
                                        sent_to.insert(acting);
                                        if let Some(released) = released.take() {
                                            let _ = released.send(());
                                        }
                                    }
                                    Err(Unsent::Unwritable(error)) => {
                                        return Err(CoordinatorError::Unwritable(error));
                                    }
                                    Err(Unsent::NotLinked) if sent.is_none() && from.is_none() => {
                                        return Err(CoordinatorError::NotLinked(acting));
                                    }
                                    Err(Unsent::Older(speaks))
                                        if sent.is_none() && from.is_none() =>
                                    {
                                        return Err(CoordinatorError::Older { acting, speaks });
                                    }
                                    Err(Unsent::NotLinked | Unsent::Older(_)) => {}
                                }
                            }
                        }
                        Next::Wait => {}
                    }
                    if look.waited >= KEEP_FORWARDED && failed.is_none() {
                        return Err(match (sent, from) {
                            (Some((to, _)), _) => CoordinatorError::NoAnswer { acting: to },
                            (None, Some(from)) => CoordinatorError::Unsettled {
                                from,
                                acting: look.acting,
                            },
                            (None, None) => CoordinatorError::NotLinked(look.acting),
                        });
                    }
                }
            }
            if let Some(error) = failed {
                let text = error.to_string();
                if logged.as_ref() != Some(&text) {
                    tracing::warn!("coordinator: {text}; looking again");
                    logged = Some(text);
                }
                if arrived.elapsed() >= KEEP_FORWARDED {
                    return Err(match sent {
                        Some((acting, _)) => CoordinatorError::NoAnswer { acting },
                        None => error.into(),
                    });
                }
            }
            tokio::time::sleep(STEP).await;
        }
    }

    /// One look at an edit that waits: reads the settings, and says what it does next and who
    /// acts. A read that fails is logged once, in `logged`. This is the one place that decides
    /// where an edit goes, for an edit with an id ([`Coordinator::run_edit`]) and one without
    /// ([`Coordinator::pass_on`]).
    async fn look(
        &self,
        from: Option<MachineId>,
        arrived: Instant,
        logged: &mut Option<String>,
    ) -> Result<Look, StoreError> {
        let (held, sender_spoke) = self.view(from, arrived).await?;
        let online = self.peers.online();
        let waited = arrived.elapsed();
        let caught_up = self.caught_up(&held, &online);
        let next = next(
            self.me,
            from,
            &held,
            &online,
            sender_spoke,
            caught_up,
            waited,
        );
        let acting = acting(&held.priority, &online);
        // A good read ends a run of the same failed one.
        *logged = None;
        Ok(Look {
            held,
            next,
            acting,
            waited,
        })
    }

    /// Passes on, or numbers here, an edit `peer` forwarded with no id, because it came from a
    /// machine before [`ANSWERS_SINCE`]. Nobody answers it, so this ends once the edit is sent
    /// on a link that speaks [`foundry_proto::messages::EDITS_SINCE`] or later, or numbered, and
    /// never sends it again. It differs from [`Coordinator::run_edit`] only in that: it has no id
    /// to look up or be answered under, so nothing here can be refused. `released` is dropped on
    /// return, which lets the next forwarded edit go on.
    async fn pass_on(
        &self,
        peer: MachineId,
        arrived: Instant,
        edit: Edit,
        released: oneshot::Sender<()>,
    ) -> Result<(), CoordinatorError> {
        let _released = released;
        let mut logged: Option<String> = None;
        loop {
            match self.look(Some(peer), arrived, &mut logged).await {
                Err(error) => {
                    let text = error.to_string();
                    if logged.as_ref() != Some(&text) {
                        tracing::warn!("coordinator: {text}; looking again");
                        logged = Some(text);
                    }
                    if arrived.elapsed() >= KEEP_FORWARDED {
                        return Err(error.into());
                    }
                }
                Ok(look) => {
                    match look.next {
                        Next::Number => return self.number(None, &edit).await.map(|_| ()),
                        Next::Send(acting) => match self.peers.send_edit(acting, None, &edit).await
                        {
                            Ok(_) => return Ok(()),
                            Err(Unsent::Unwritable(error)) => {
                                return Err(CoordinatorError::Unwritable(error));
                            }
                            Err(Unsent::NotLinked | Unsent::Older(_)) => {}
                        },
                        Next::Wait => {}
                    }
                    if look.waited >= KEEP_FORWARDED {
                        return Err(CoordinatorError::Unsettled {
                            from: peer,
                            acting: look.acting,
                        });
                    }
                }
            }
            tokio::time::sleep(STEP).await;
        }
    }

    /// Registers `id` as an edit this machine waits on, until the guard drops. Edits that wait
    /// on one id are counted, so the answer stays until the last of them is done.
    fn wait_on(&self, id: EditId) -> Waiting<'_> {
        lock(&self.answers).entry(id).or_insert((0, None)).0 += 1;
        Waiting {
            answers: &self.answers,
            id,
        }
    }

    /// Gives the edit the next number, on the copy held at that moment, unless that copy
    /// already holds a stamp for its `id`, which is returned.
    async fn number(&self, id: Option<EditId>, edit: &Edit) -> Result<Stamp, CoordinatorError> {
        let me = self.me;
        let edit = edit.clone();
        let stamp = on_store(&self.store, move |store| {
            let held = store.settings()?;
            if let Some(stamp) = id.and_then(|id| held.numbered.get(&id)) {
                return Ok(*stamp);
            }
            let mut next = numbered(&held, &edit, me);
            let stamp = Stamp {
                seq: next.seq,
                by: me,
            };
            if let Some(id) = id {
                remember(&mut next, id, stamp);
            }
            store.apply_settings(next)?;
            Ok::<_, StoreError>(stamp)
        })
        .await?;
        Ok(stamp)
    }

    /// The settings held, and whether a copy of them was taken from `from` after `arrived`,
    /// read together under the store's lock: a copy counted is in the settings returned.
    async fn view(
        &self,
        from: Option<MachineId>,
        arrived: Instant,
    ) -> Result<(SharedSettings, bool), StoreError> {
        let taken = Arc::clone(&self.taken);
        on_store(&self.store, move |store| {
            let held = store.settings()?;
            let spoke = from.is_some_and(|from| {
                lock(&taken)
                    .get(&from)
                    .is_some_and(|taken| *taken > arrived)
            });
            Ok((held, spoke))
        })
        .await
    }

    async fn held(&self) -> Result<SharedSettings, StoreError> {
        on_store(&self.store, |store| store.settings()).await
    }

    /// Whether this machine holds what it needs to number: a copy from every other machine it
    /// can reach, and, in its first [`OFFLINE_AFTER`], from every machine `held` lists, so a
    /// machine that has just started and hears from no one yet does not number on an old copy.
    /// A machine linked only in a protocol older than edits sends no copy, so none is needed
    /// from it.
    fn caught_up(&self, held: &SharedSettings, online: &BTreeSet<MachineId>) -> bool {
        let older = self.peers.older();
        let taken = lock(&self.taken);
        let has = |id: &MachineId| *id == self.me || taken.contains_key(id) || older.contains(id);
        let reachable = online.iter().all(has);
        let listed = held.machines.iter().all(|m| has(&m.id));
        reachable && (listed || self.started.elapsed() >= OFFLINE_AFTER)
    }
}

/// An edit this machine waits on, in [`Coordinator::answers`], removed when this drops.
struct Waiting<'a> {
    answers: &'a Mutex<BTreeMap<EditId, (usize, Option<Outcome>)>>,
    id: EditId,
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        let mut answers = lock(self.answers);
        if let Some((waiting, _)) = answers.get_mut(&self.id) {
            *waiting -= 1;
            if *waiting == 0 {
                answers.remove(&self.id);
            }
        }
    }
}

/// A forwarded edit from one machine with a task queued or running, in [`Coordinator::forwarding`], removed when
/// this drops.
struct Queued {
    coordinator: Arc<Coordinator>,
    peer: MachineId,
    id: EditId,
}

impl Queued {
    /// `None` when `id` already has one.
    fn new(coordinator: &Arc<Coordinator>, peer: MachineId, id: EditId) -> Option<Self> {
        lock(&coordinator.forwarding)
            .insert((peer, id))
            .then(|| Self {
                coordinator: Arc::clone(coordinator),
                peer,
                id,
            })
    }
}

impl Drop for Queued {
    fn drop(&mut self) {
        lock(&self.coordinator.forwarding).remove(&(self.peer, self.id));
    }
}

/// What one look at a waiting edit found.
struct Look {
    held: SharedSettings,
    next: Next,
    acting: MachineId,
    waited: Duration,
}

/// What a waiting edit does next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Next {
    /// Take the next number here.
    Number,
    /// Go to this machine, which acts.
    Send(MachineId),
    /// Look again in a [`STEP`].
    Wait,
}

/// What an edit on machine `me` does next, from one view: `held`, and `sender_spoke`, whether
/// a copy was taken from `from` (the machine that forwarded it, `None` for `me`'s own) after
/// the edit arrived, read together; `online`, who `me` can reach; `caught_up`, whether `me`
/// holds every copy it needs; `waited`, how long since the edit arrived.
///
/// The acting machine numbers once caught up or after [`OFFLINE_AFTER`]. Any other machine
/// sends the edit to the acting one, except to the machine that forwarded it: the two may
/// still disagree on who acts, so it goes back only once a copy from that machine, taken
/// since the edit arrived, still leaves it acting, or after [`OFFLINE_AFTER`].
fn next(
    me: MachineId,
    from: Option<MachineId>,
    held: &SharedSettings,
    online: &BTreeSet<MachineId>,
    sender_spoke: bool,
    caught_up: bool,
    waited: Duration,
) -> Next {
    let acting = acting(&held.priority, online);
    if acting == me {
        if caught_up || waited >= OFFLINE_AFTER {
            Next::Number
        } else {
            Next::Wait
        }
    } else if Some(acting) != from || sender_spoke || waited >= OFFLINE_AFTER {
        Next::Send(acting)
    } else {
        Next::Wait
    }
}

/// The machine that holds the coordinator role: the first in `priority` that is in
/// `reachable`, else the lowest id in `reachable`. `reachable` holds the asking machine, so is
/// never empty in use; when it is, the first in `priority` acts, or the nil id.
pub fn acting(priority: &[MachineId], reachable: &BTreeSet<MachineId>) -> MachineId {
    priority
        .iter()
        .find(|id| reachable.contains(id))
        .or_else(|| reachable.first())
        .or_else(|| priority.first())
        .copied()
        .unwrap_or(Stamp::NONE.by)
}

/// `held` with `edit` applied as the next number, given by `me`: the setting takes the stamp
/// `{held.seq + 1, me}`.
pub fn numbered(held: &SharedSettings, edit: &Edit, me: MachineId) -> SharedSettings {
    let seq = held.seq.saturating_add(1);
    let mut next = held.clone();
    let value = edit.value().map(Value::Setting);
    put(&mut next, &edit.key(), value, Stamp { seq, by: me });
    next.seen.insert(me, seq);
    next.seq = seq;
    next
}

/// Notes in `settings` that the edit sent under `id` was numbered with `stamp`, keeping the
/// higher stamp when `id` is already there, and forgets the ids that are [`NUMBERED_KEPT`]
/// numbers or more below the newest one held.
///
/// The bound is the newest id held, not `settings.seq`: it is the same in every copy that holds
/// those ids, so copies give the same result whatever order they merge in.
pub fn remember(settings: &mut SharedSettings, id: EditId, stamp: Stamp) {
    keep_higher(&mut settings.numbered, id, stamp);
    forget_old(&mut settings.numbered);
}

/// Notes `stamp` for `id` in `numbered`, unless it holds a higher one.
fn keep_higher(numbered: &mut BTreeMap<EditId, Stamp>, id: EditId, stamp: Stamp) {
    let held = numbered.entry(id).or_insert(stamp);
    *held = (*held).max(stamp);
}

/// Drops every id whose number is [`NUMBERED_KEPT`] or more below the highest one held.
fn forget_old(numbered: &mut BTreeMap<EditId, Stamp>) {
    let Some(top) = numbered.values().map(|stamp| stamp.seq).max() else {
        return;
    };
    numbered.retain(|_, stamp| stamp.seq.saturating_add(NUMBERED_KEPT) > top);
}

/// One copy of the settings from two, setting by setting: the higher stamp's value holds. A
/// setting both copies edited where neither had seen the other's edit also gets a "setting
/// overwritten" item holding both values, stamped as the edit that held, or as the dismissal of
/// an earlier item about that setting when that is later. `seen` is the higher of each.
///
/// The result's `seq` is the higher of the two, plus one unless the result is the settings of
/// every copy that holds that `seq`. That is one more case than "differs from both": a result
/// that is only the lower copy's settings also takes a new number. A store takes a copy only
/// with a higher `seq` than it holds, so a store holding the top copy would refuse such a
/// result if it kept the top `seq`, and the lower copy's edits would never reach it.
pub fn merge(a: &SharedSettings, b: &SharedSettings) -> SharedSettings {
    let mut out = SharedSettings::default();
    let mut conflicts = Vec::new();
    for key in keys(a).union(&keys(b)) {
        let (sa, sb) = (stamp(a, key), stamp(b, key));
        let (va, vb) = (get(a, key), get(b, key));
        let a_holds = match sa.cmp(&sb) {
            std::cmp::Ordering::Greater => true,
            std::cmp::Ordering::Less => false,
            // One edit, or two copies no edit has stamped: pick by the value, the same way on
            // every machine.
            std::cmp::Ordering::Equal => rank(&va) >= rank(&vb),
        };
        let (held, lost, stamp, lost_stamp) = if a_holds {
            (va, vb, sa, sb)
        } else {
            (vb, va, sb, sa)
        };
        // Two sides that set one value raise nothing: no value was lost.
        if let StampKey::Setting(setting) = key
            && held != lost
            && !covers(&a.seen, sb)
            && !covers(&b.seen, sa)
        {
            let item = Overwritten {
                kept: self::setting(&held),
                lost: self::setting(&lost),
                lost_stamp,
            };
            conflicts.push((setting.clone(), item, stamp));
        }
        put(&mut out, key, held, stamp);
    }
    // An item from this merge replaces one held for the same setting only when it is about a
    // later edit kept or, for the same edit kept, a later edit lost: so with three or more
    // sides, every order of merging keeps the latest edit each side lost to.
    //
    // A conflict found here is news to both copies: a copy that held an item about it had
    // seen both edits, so would raise none. A dismissal held for the setting is therefore of
    // an earlier item, and the new item shows, stamped no lower than the dismissal so the
    // dismissing copy does not remove it again when they meet.
    for (setting, item, stamp) in conflicts {
        let held_lost = out.overwritten.get(&setting).map(|i| i.lost_stamp);
        let key = StampKey::Overwritten(setting);
        let held = self::stamp(&out, &key);
        let replaces = match held_lost {
            None => true,
            Some(held_lost) => (stamp, item.lost_stamp) > (held, held_lost),
        };
        if replaces {
            put(&mut out, &key, Some(Value::Item(item)), stamp.max(held));
        }
    }
    out.numbered = a.numbered.clone();
    for (id, stamp) in &b.numbered {
        keep_higher(&mut out.numbered, *id, *stamp);
    }
    forget_old(&mut out.numbered);
    out.seen = a.seen.clone();
    for (by, seq) in &b.seen {
        let seen = out.seen.entry(*by).or_default();
        *seen = (*seen).max(*seq);
    }
    let top = a.seq.max(b.seq);
    let is_top = [a, b]
        .iter()
        .filter(|copy| copy.seq == top)
        .all(|copy| same_settings(copy, &out));
    out.seq = if is_top { top } else { top.saturating_add(1) };
    out
}

/// Whether two copies hold the same settings, whatever their `seq`.
fn same_settings(a: &SharedSettings, b: &SharedSettings) -> bool {
    SharedSettings {
        seq: 0,
        ..a.clone()
    } == SharedSettings {
        seq: 0,
        ..b.clone()
    }
}

/// Whether a copy that has seen `seen` includes the edit stamped `stamp`.
fn covers(seen: &BTreeMap<MachineId, u64>, stamp: Stamp) -> bool {
    stamp == Stamp::NONE || seen.get(&stamp.by).is_some_and(|seq| *seq >= stamp.seq)
}

/// The value of one setting: a plain setting's, or an overwritten item.
#[derive(Clone, Debug, PartialEq, Serialize)]
enum Value {
    Setting(SettingValue),
    Item(Overwritten),
}

fn setting(value: &Option<Value>) -> Option<SettingValue> {
    match value {
        Some(Value::Setting(v)) => Some(v.clone()),
        _ => None,
    }
}

/// How two values with one stamp are ordered, the same way on every machine: of two items, the
/// one about the later edit lost holds; otherwise, and on a tie, by the value as JSON text.
fn rank(value: &Option<Value>) -> (Option<Stamp>, String) {
    let lost = match value {
        Some(Value::Item(item)) => Some(item.lost_stamp),
        _ => None,
    };
    (lost, text(value))
}

/// `value` as JSON text, to order two values the same way on every machine.
fn text(value: &Option<Value>) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

/// Every setting `settings` holds a value or a stamp for.
fn keys(settings: &SharedSettings) -> BTreeSet<StampKey> {
    let mut keys: BTreeSet<StampKey> = settings.stamps.keys().cloned().collect();
    let mut plain: Vec<SettingKey> = Vec::new();
    plain.extend(settings.machines.iter().map(|m| SettingKey::Machine(m.id)));
    plain.extend(settings.projects.iter().map(|p| SettingKey::Project(p.id)));
    plain.extend(settings.limits.keys().map(|id| SettingKey::Limits(*id)));
    if !settings.priority.is_empty() {
        plain.push(SettingKey::Priority);
    }
    keys.extend(plain.into_iter().map(StampKey::Setting));
    keys.extend(
        settings
            .overwritten
            .keys()
            .cloned()
            .map(StampKey::Overwritten),
    );
    keys
}

fn stamp(settings: &SharedSettings, key: &StampKey) -> Stamp {
    settings.stamps.get(key).copied().unwrap_or(Stamp::NONE)
}

/// The value `settings` holds for `key`, `None` when it holds none. An empty priority order is
/// none.
fn get(settings: &SharedSettings, key: &StampKey) -> Option<Value> {
    let key = match key {
        StampKey::Overwritten(key) => {
            return settings.overwritten.get(key).cloned().map(Value::Item);
        }
        StampKey::Setting(key) => key,
    };
    let value = match key {
        SettingKey::Machine(id) => settings
            .machines
            .iter()
            .find(|m| m.id == *id)
            .cloned()
            .map(SettingValue::Machine),
        SettingKey::Priority => (!settings.priority.is_empty())
            .then(|| SettingValue::Priority(settings.priority.clone())),
        SettingKey::Project(id) => settings
            .projects
            .iter()
            .find(|p| p.id == *id)
            .cloned()
            .map(SettingValue::Project),
        SettingKey::Limits(id) => settings.limits.get(id).cloned().map(SettingValue::Limits),
    };
    value.map(Value::Setting)
}

/// Sets `key` to `value` (`None` removes it) with `stamp`. Machines and projects are kept in
/// id order, so two machines holding the same settings hold the same lists. A value of the
/// wrong kind for `key` is taken as none.
fn put(settings: &mut SharedSettings, key: &StampKey, value: Option<Value>, stamp: Stamp) {
    match key {
        StampKey::Setting(SettingKey::Machine(id)) => {
            settings.machines.retain(|m| m.id != *id);
            if let Some(Value::Setting(SettingValue::Machine(m))) = value
                && m.id == *id
            {
                settings.machines.push(m);
                settings.machines.sort_by_key(|m| m.id);
            }
        }
        StampKey::Setting(SettingKey::Priority) => {
            settings.priority = match value {
                Some(Value::Setting(SettingValue::Priority(order))) => order,
                _ => Vec::new(),
            };
        }
        StampKey::Setting(SettingKey::Project(id)) => {
            settings.projects.retain(|p| p.id != *id);
            if let Some(Value::Setting(SettingValue::Project(p))) = value
                && p.id == *id
            {
                settings.projects.push(p);
                settings.projects.sort_by_key(|p| p.id);
            }
        }
        StampKey::Setting(SettingKey::Limits(id)) => match value {
            Some(Value::Setting(SettingValue::Limits(limits))) => {
                settings.limits.insert(*id, limits);
            }
            _ => {
                settings.limits.remove(id);
            }
        },
        StampKey::Overwritten(inner) => match value {
            Some(Value::Item(item)) => {
                settings.overwritten.insert(inner.clone(), item);
            }
            _ => {
                settings.overwritten.remove(inner);
            }
        },
    }
    if stamp == Stamp::NONE {
        settings.stamps.remove(key);
    } else {
        settings.stamps.insert(key.clone(), stamp);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::time::Duration;

    use foundry_proto::ids::{MachineId, ProjectId};
    use foundry_proto::settings::{
        Edit, EditId, Limits, Machine, Overwritten, Project, SettingKey, SettingValue,
        SharedSettings, Stamp, StampKey,
    };

    use super::*;
    use crate::sync::{Conn, Dialer};
    use foundry_proto::settings::Machine as SettingsMachine;
    use futures::future::BoxFuture;

    struct NoDial;

    impl Dialer for NoDial {
        fn dial(&self, _: &SettingsMachine) -> BoxFuture<'static, std::io::Result<Conn>> {
            Box::pin(async { Err(std::io::Error::other("no peers in these tests")) })
        }
    }

    /// A coordinator alone on its own store: it acts, and has nothing to catch up on.
    fn alone() -> (tempfile::TempDir, Coordinator) {
        let dir = tempfile::tempdir().unwrap();
        let me = MachineId::random();
        let store = Arc::new(Mutex::new(
            Store::open(dir.path().join("foundry.db")).unwrap(),
        ));
        let peers = Peers::new(me, store.clone(), Arc::new(NoDial));
        (dir, Coordinator::new(me, store, peers))
    }

    /// Machine ids in a known order: `id(1) < id(2) < …`.
    fn id(n: u128) -> MachineId {
        MachineId(uuid::Uuid::from_u128(n))
    }

    fn reachable(ids: &[MachineId]) -> BTreeSet<MachineId> {
        ids.iter().copied().collect()
    }

    #[test]
    fn the_first_reachable_machine_in_the_order_acts() {
        let (a, b, c) = (id(3), id(1), id(2));
        let order = [a, b, c];
        assert_eq!(acting(&order, &reachable(&[a, b, c])), a);
        assert_eq!(acting(&order, &reachable(&[b, c])), b);
        assert_eq!(acting(&order, &reachable(&[c])), c);
    }

    #[test]
    fn with_no_reachable_machine_in_the_order_the_lowest_reachable_id_acts() {
        let (a, b, c) = (id(3), id(1), id(2));
        assert_eq!(acting(&[a], &reachable(&[b, c])), b);
        assert_eq!(acting(&[], &reachable(&[c, a])), c);
    }

    const A: u128 = 1;
    const B: u128 = 2;

    fn project(n: u128, name: &str) -> Project {
        Project {
            id: ProjectId(uuid::Uuid::from_u128(n)),
            name: name.into(),
            repo: format!("https://example.invalid/{name}.git"),
        }
    }

    fn limits(max_sessions: u32) -> Limits {
        Limits {
            max_sessions,
            max_load_per_core: 1.0,
            min_free_disk: 1,
        }
    }

    /// Two machines, both listed, `a` first, as both held them before any split.
    fn base() -> SharedSettings {
        let machines = [id(A), id(B)]
            .iter()
            .enumerate()
            .map(|(i, id)| Machine {
                id: *id,
                name: format!("m{i}"),
                address: format!("100.64.0.{i}"),
            })
            .collect();
        SharedSettings {
            seq: 1,
            machines,
            priority: vec![id(A), id(B)],
            ..SharedSettings::default()
        }
    }

    fn set_project(n: u128, name: &str) -> Edit {
        Edit::SetProject {
            project: project(n, name),
        }
    }

    fn project_named(settings: &SharedSettings, n: u128) -> Option<String> {
        settings
            .projects
            .iter()
            .find(|p| p.id.0 == uuid::Uuid::from_u128(n))
            .map(|p| p.name.clone())
    }

    /// Merges both ways, checks both orders agree and merging again changes nothing.
    fn merged(a: &SharedSettings, b: &SharedSettings) -> SharedSettings {
        let ab = merge(a, b);
        let ba = merge(b, a);
        assert_eq!(ab, ba, "the merge depends on its order");
        assert_eq!(
            merge(&ab, a),
            ab,
            "merging a side in again changes the result"
        );
        assert_eq!(
            merge(&ab, b),
            ab,
            "merging a side in again changes the result"
        );
        assert_eq!(merge(&ab, &ab), ab, "a merge with itself changes it");
        ab
    }

    // --- what a waiting edit does next ---

    const EARLY: Duration = Duration::from_secs(1);

    #[test]
    fn the_acting_machine_numbers_once_caught_up_or_after_the_offline_limit() {
        let (held, all) = (base(), reachable(&[id(A), id(B)]));
        assert_eq!(
            next(id(A), None, &held, &all, false, true, EARLY),
            Next::Number
        );
        assert_eq!(
            next(id(A), None, &held, &all, false, false, EARLY),
            Next::Wait
        );
        let late = OFFLINE_AFTER;
        assert_eq!(
            next(id(A), None, &held, &all, false, false, late),
            Next::Number
        );
        // A forwarded edit is numbered the same way.
        let from = Some(id(B));
        assert_eq!(
            next(id(A), from, &held, &all, false, true, EARLY),
            Next::Number
        );
    }

    #[test]
    fn an_edit_on_a_machine_that_does_not_act_goes_to_the_one_that_does() {
        let (held, all) = (base(), reachable(&[id(A), id(B)]));
        assert_eq!(
            next(id(B), None, &held, &all, false, false, EARLY),
            Next::Send(id(A))
        );
        let other = Some(id(9));
        assert_eq!(
            next(id(B), other, &held, &all, false, false, EARLY),
            Next::Send(id(A))
        );
        // With the first machine out of reach, the next one acts.
        let alone = reachable(&[id(B)]);
        assert_eq!(
            next(id(B), None, &held, &alone, false, true, EARLY),
            Next::Number
        );
    }

    #[test]
    fn an_edit_from_the_acting_machine_waits_until_it_sends_a_copy_or_the_offline_limit() {
        let (held, all) = (base(), reachable(&[id(A), id(B)]));
        let from = Some(id(A));
        assert_eq!(
            next(id(B), from, &held, &all, false, true, EARLY),
            Next::Wait
        );
        assert_eq!(
            next(id(B), from, &held, &all, true, true, EARLY),
            Next::Send(id(A))
        );
        let late = OFFLINE_AFTER;
        assert_eq!(
            next(id(B), from, &held, &all, false, true, late),
            Next::Send(id(A))
        );
    }

    #[test]
    fn a_copy_from_the_sender_that_hands_this_machine_the_role_numbers_the_edit_here() {
        // The sender's copy, counted as a copy since the edit arrived, put this machine first.
        let mut held = base();
        held.priority = vec![id(B), id(A)];
        let all = reachable(&[id(A), id(B)]);
        assert_eq!(
            next(id(B), Some(id(A)), &held, &all, true, true, EARLY),
            Next::Number
        );
    }

    #[test]
    fn an_edit_takes_the_next_number_and_stamps_its_setting() {
        let held = base();
        let next = numbered(&held, &set_project(7, "p"), id(B));
        assert_eq!(next.seq, 2);
        assert_eq!(next.seen, [(id(B), 2)].into());
        assert_eq!(
            next.stamps[&StampKey::Setting(SettingKey::Project(project(7, "p").id))],
            Stamp { seq: 2, by: id(B) }
        );
        assert_eq!(project_named(&next, 7).as_deref(), Some("p"));
        assert_eq!(next.machines, held.machines);
        assert_eq!(next.priority, held.priority);
    }

    #[test]
    fn a_removal_keeps_its_stamp() {
        let held = numbered(&base(), &set_project(7, "p"), id(A));
        let id7 = project(7, "p").id;
        let next = numbered(&held, &Edit::RemoveProject { id: id7 }, id(A));
        assert_eq!(project_named(&next, 7), None);
        assert_eq!(
            next.stamps[&StampKey::Setting(SettingKey::Project(id7))],
            Stamp { seq: 3, by: id(A) }
        );
    }

    #[test]
    fn each_kind_of_edit_changes_its_setting() {
        let mut held = base();
        let c = Machine {
            id: id(9),
            name: "c".into(),
            address: "100.64.0.9".into(),
        };
        held = numbered(&held, &Edit::SetMachine { machine: c.clone() }, id(A));
        assert!(held.machines.contains(&c));
        held = numbered(&held, &Edit::Priority { order: vec![id(9)] }, id(A));
        assert_eq!(held.priority, vec![id(9)]);
        held = numbered(
            &held,
            &Edit::SetLimits {
                machine: id(9),
                limits: limits(3),
            },
            id(A),
        );
        assert_eq!(held.limits[&id(9)], limits(3));
        held = numbered(&held, &Edit::RemoveLimits { machine: id(9) }, id(A));
        assert!(!held.limits.contains_key(&id(9)));
        held = numbered(&held, &Edit::RemoveMachine { id: id(9) }, id(A));
        assert!(!held.machines.contains(&c));
        assert_eq!(held.seq, 6);
    }

    #[test]
    fn after_a_split_the_side_with_the_higher_number_holds_and_an_item_shows_both() {
        let id7 = project(7, "x").id;
        // Side a: one edit. Side b: an unrelated edit, then the same setting, later.
        let a = numbered(&base(), &set_project(7, "from a"), id(A));
        let b = numbered(&base(), &Edit::RemoveLimits { machine: id(B) }, id(B));
        let b = numbered(&b, &set_project(7, "from b"), id(B));
        let out = merged(&a, &b);
        assert_eq!(project_named(&out, 7).as_deref(), Some("from b"));
        assert_eq!(
            out.overwritten[&SettingKey::Project(id7)],
            Overwritten {
                kept: Some(SettingValue::Project(project(7, "from b"))),
                lost: Some(SettingValue::Project(project(7, "from a"))),
                lost_stamp: Stamp { seq: 2, by: id(A) },
            }
        );
        assert_eq!(
            out.stamps[&StampKey::Overwritten(SettingKey::Project(id7))],
            Stamp { seq: 3, by: id(B) }
        );
    }

    #[test]
    fn an_equal_number_goes_to_the_lower_machine_id() {
        let a = numbered(&base(), &set_project(7, "from a"), id(A));
        let b = numbered(&base(), &set_project(7, "from b"), id(B));
        let out = merged(&a, &b);
        assert_eq!(project_named(&out, 7).as_deref(), Some("from a"));
        assert_eq!(out.overwritten.len(), 1);
    }

    #[test]
    fn three_sides_of_a_split_reach_one_result_in_any_order_and_the_item_keeps_the_runner_up() {
        const C: u128 = 3;
        let id7 = project(7, "x").id;
        // Three sides each edit project 7, numbered 2 by a, 3 by b and 4 by c. As text, a's
        // value sorts after b's, so the item cannot reach b's value by text alone.
        let a = numbered(&base(), &set_project(7, "from the first side"), id(A));
        let b = numbered(&base(), &set_project(8, "b's other"), id(B));
        let b = numbered(&b, &set_project(7, "from b"), id(B));
        let mut c = base();
        for n in [9, 10] {
            c = numbered(&c, &set_project(n, "c's other"), id(C));
        }
        let c = numbered(&c, &set_project(7, "from c"), id(C));

        let copies = [("a", &a), ("b", &b), ("c", &c)];
        let mut results = Vec::new();
        for [x, y, z] in [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ] {
            let ((nx, x), (ny, y), (nz, z)) = (copies[x], copies[y], copies[z]);
            results.push((format!("({nx} {ny}) {nz}"), merge(&merge(x, y), z)));
            // Two machines that each met the third side meet each other.
            results.push((
                format!("({nx} {nz}) ({ny} {nz})"),
                merge(&merge(x, z), &merge(y, z)),
            ));
        }
        let (_, first) = &results[0];
        for (order, result) in &results {
            assert!(
                same_settings(result, first),
                "{order} gives {:?}, not {:?}",
                result.overwritten,
                first.overwritten
            );
        }
        assert_eq!(project_named(first, 7).as_deref(), Some("from c"));
        // The item shows the value that held and the latest edit it replaced.
        assert_eq!(
            first.overwritten[&SettingKey::Project(id7)],
            Overwritten {
                kept: Some(SettingValue::Project(project(7, "from c"))),
                lost: Some(SettingValue::Project(project(7, "from b"))),
                lost_stamp: Stamp { seq: 3, by: id(B) },
            }
        );
    }

    #[test]
    fn edits_to_different_settings_on_each_side_both_hold_with_no_item() {
        let a = numbered(&base(), &set_project(7, "from a"), id(A));
        let b = numbered(
            &base(),
            &Edit::SetLimits {
                machine: id(B),
                limits: limits(2),
            },
            id(B),
        );
        let out = merged(&a, &b);
        assert_eq!(project_named(&out, 7).as_deref(), Some("from a"));
        assert_eq!(out.limits[&id(B)], limits(2));
        assert!(out.overwritten.is_empty());
        assert_eq!(out.seen, [(id(A), 2), (id(B), 2)].into());
        // The result is neither input, so it takes a number above both.
        assert_eq!(out.seq, 3);
    }

    #[test]
    fn a_takeover_that_edits_what_the_old_coordinator_edited_is_no_conflict() {
        let old = numbered(&base(), &set_project(7, "by a"), id(A));
        // B took over holding a's copy, and edits the same project.
        let new = numbered(&old, &set_project(7, "by b"), id(B));
        let out = merged(&old, &new);
        assert_eq!(project_named(&out, 7).as_deref(), Some("by b"));
        assert!(out.overwritten.is_empty());
        assert_eq!(out, new);
    }

    #[test]
    fn a_machine_that_was_only_behind_takes_the_newer_copy_unchanged() {
        let behind = base();
        let ahead = numbered(&base(), &set_project(7, "p"), id(A));
        let ahead = numbered(&ahead, &set_project(7, "q"), id(A));
        let out = merged(&behind, &ahead);
        assert_eq!(out, ahead);
    }

    #[test]
    fn a_removal_on_one_side_holds_unless_the_other_side_edited_it_later() {
        let synced = numbered(&base(), &set_project(7, "p"), id(A));
        let id7 = project(7, "p").id;
        let removed = numbered(&synced, &Edit::RemoveProject { id: id7 }, id(A));
        // The other side made no edit to it.
        let other = numbered(&synced, &Edit::RemoveLimits { machine: id(B) }, id(B));
        let out = merged(&removed, &other);
        assert_eq!(project_named(&out, 7), None);
        assert!(out.overwritten.is_empty());
        // The other side edited it later: that edit holds, and both values show.
        let other = numbered(&other, &set_project(7, "kept"), id(B));
        let out = merged(&removed, &other);
        assert_eq!(project_named(&out, 7).as_deref(), Some("kept"));
        assert_eq!(
            out.overwritten[&SettingKey::Project(id7)],
            Overwritten {
                kept: Some(SettingValue::Project(project(7, "kept"))),
                lost: None,
                lost_stamp: Stamp { seq: 3, by: id(A) },
            }
        );
    }

    #[test]
    fn a_dismissed_item_stays_dismissed_when_it_meets_a_copy_that_still_holds_it() {
        let a = numbered(&base(), &set_project(7, "from a"), id(A));
        let b = numbered(&base(), &set_project(7, "from b"), id(B));
        let joined = merged(&a, &b);
        let key = SettingKey::Project(project(7, "p").id);
        assert!(joined.overwritten.contains_key(&key));
        let dismissed = numbered(
            &joined,
            &Edit::Dismiss {
                setting: key.clone(),
            },
            id(A),
        );
        assert!(dismissed.overwritten.is_empty());
        let out = merged(&dismissed, &joined);
        assert!(out.overwritten.is_empty());
        assert_eq!(out, dismissed);
        // So does a side that held only one of the split's copies.
        let out = merged(&dismissed, &b);
        assert!(out.overwritten.is_empty());
        assert_eq!(project_named(&out, 7).as_deref(), Some("from a"));
    }

    #[test]
    fn a_dismissal_made_during_a_split_does_not_hide_a_conflict_the_split_raised() {
        let a = numbered(&base(), &set_project(7, "from a"), id(A));
        let b = numbered(&base(), &set_project(7, "from b"), id(B));
        let joined = merged(&a, &b);
        let key = SettingKey::Project(project(7, "p").id);
        // Split again. One side edits the project, then dismisses the old item; the other
        // side edits the project too.
        let x = numbered(&joined, &set_project(7, "x"), id(A));
        let x = numbered(
            &x,
            &Edit::Dismiss {
                setting: key.clone(),
            },
            id(A),
        );
        let y = numbered(&joined, &set_project(7, "y"), id(B));
        let out = merged(&x, &y);
        assert_eq!(project_named(&out, 7).as_deref(), Some("x"));
        assert_eq!(
            out.overwritten.get(&key),
            Some(&Overwritten {
                kept: Some(SettingValue::Project(project(7, "x"))),
                lost: Some(SettingValue::Project(project(7, "y"))),
                lost_stamp: y.stamps[&StampKey::Setting(key.clone())],
            })
        );
        // The new item can itself be dismissed.
        let dismissed = numbered(&out, &Edit::Dismiss { setting: key }, id(A));
        assert!(merged(&dismissed, &y).overwritten.is_empty());
        assert!(merged(&dismissed, &x).overwritten.is_empty());
    }

    #[test]
    fn copies_that_differ_only_in_the_order_of_their_lists_merge_to_one_order() {
        let mut a = numbered(&base(), &set_project(9, "nine"), id(A));
        a = numbered(&a, &set_project(8, "eight"), id(A));
        let mut b = a.clone();
        b.projects.reverse();
        b.machines.reverse();
        assert_eq!(merge(&a, &b).projects, merge(&b, &a).projects);
        assert_eq!(merge(&a, &b).machines, merge(&b, &a).machines);
    }

    #[test]
    fn the_merged_seq_is_the_higher_one_when_the_result_is_that_copy() {
        let a = numbered(&base(), &set_project(7, "p"), id(A));
        let mut same = a.clone();
        same.seq = 9;
        let out = merged(&a, &same);
        assert_eq!(out.seq, 9);
        // A copy with a higher seq that the merge changes is passed by one.
        let mut stale = base();
        stale.seq = 9;
        let out = merged(&a, &stale);
        assert_eq!(project_named(&out, 7).as_deref(), Some("p"));
        assert_eq!(out.seq, 10);
    }

    // --- the ids of numbered edits ---

    #[test]
    fn a_merge_keeps_the_numbered_edit_ids_of_both_copies() {
        let (x, y) = (EditId::random(), EditId::random());
        let mut a = numbered(&base(), &set_project(7, "from a"), id(A));
        remember(&mut a, x, Stamp { seq: 2, by: id(A) });
        let mut b = numbered(&base(), &set_project(8, "from b"), id(B));
        remember(&mut b, y, Stamp { seq: 2, by: id(B) });
        let out = merged(&a, &b);
        assert_eq!(
            out.numbered,
            [
                (x, Stamp { seq: 2, by: id(A) }),
                (y, Stamp { seq: 2, by: id(B) })
            ]
            .into()
        );
    }

    #[test]
    fn one_id_numbered_on_two_sides_of_a_split_keeps_the_higher_stamp() {
        let x = EditId::random();
        let mut a = numbered(&base(), &set_project(7, "p"), id(A));
        remember(&mut a, x, Stamp { seq: 2, by: id(A) });
        let mut b = numbered(&base(), &Edit::RemoveLimits { machine: id(B) }, id(B));
        b = numbered(&b, &set_project(7, "p"), id(B));
        remember(&mut b, x, Stamp { seq: 3, by: id(B) });
        assert_eq!(merged(&a, &b).numbered[&x], Stamp { seq: 3, by: id(B) });
    }

    #[test]
    fn an_id_numbered_far_below_the_newest_is_forgotten_whatever_the_order() {
        let (old, new) = (EditId::random(), EditId::random());
        let low = Stamp { seq: 2, by: id(A) };
        let high = Stamp {
            seq: 2 + NUMBERED_KEPT,
            by: id(B),
        };
        let mut a = base();
        a.seq = low.seq;
        remember(&mut a, old, low);
        let mut b = base();
        b.seq = high.seq;
        remember(&mut b, new, high);
        let out = merged(&a, &b);
        assert_eq!(out.numbered, [(new, high)].into());

        // One number closer, both are kept.
        let mut near = b.clone();
        near.numbered.clear();
        remember(
            &mut near,
            new,
            Stamp {
                seq: high.seq - 1,
                ..high
            },
        );
        assert_eq!(merged(&a, &near).numbered.len(), 2);

        // Remembering a newer id forgets one that falls out of the window.
        remember(&mut a, new, high);
        assert_eq!(a.numbered, [(new, high)].into());
    }

    #[test]
    fn copies_with_no_stamps_merge_with_no_item() {
        let mut a = base();
        a.projects = vec![project(7, "a")];
        let mut b = base();
        b.projects = vec![project(7, "b")];
        let out = merged(&a, &b);
        assert!(out.overwritten.is_empty());
        assert_eq!(out.projects.len(), 1);
    }

    // --- waiting on an id, and numbering one ---

    #[tokio::test]
    async fn two_waiters_on_one_id_each_read_its_answer_after_the_first_stops_waiting() {
        let (_dir, coordinator) = alone();
        let id = EditId::random();
        let first = coordinator.wait_on(id);
        let _second = coordinator.wait_on(id);
        drop(first);
        let outcome = Outcome::Refused {
            reason: "no".into(),
        };
        coordinator.take_answer(id, outcome.clone());
        let held = lock(&coordinator.answers)
            .get(&id)
            .and_then(|(_, outcome)| outcome.clone());
        assert_eq!(held, Some(outcome));
    }

    #[tokio::test]
    async fn one_id_made_twice_at_once_is_numbered_once() {
        let (_dir, coordinator) = alone();
        let id = EditId::random();
        let before = coordinator.held().await.unwrap().seq;
        let (own, forwarded) = tokio::join!(
            coordinator.edit_from(None, Instant::now(), id, set_project(7, "p")),
            coordinator.edit_from(
                Some(MachineId::random()),
                Instant::now(),
                id,
                set_project(7, "p")
            ),
        );
        assert_eq!(own.unwrap(), forwarded.unwrap());
        assert_eq!(coordinator.held().await.unwrap().seq, before + 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_forwarded_edit_whose_settings_cannot_be_read_until_the_limit_ends_as_a_store_error()
    {
        let (dir, coordinator) = alone();
        lock(&coordinator.store)
            .apply_settings(SharedSettings {
                seq: 1,
                ..SharedSettings::default()
            })
            .unwrap();
        rusqlite::Connection::open(dir.path().join("foundry.db"))
            .unwrap()
            .execute("UPDATE settings SET settings = 'unreadable'", [])
            .unwrap();
        let error = coordinator
            .edit_from(
                Some(MachineId::random()),
                Instant::now(),
                EditId::random(),
                set_project(7, "unread"),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, CoordinatorError::Store(_)), "{error}");
    }

    /// A coordinator alone, holding settings, whose store then refuses every write.
    fn refusing_writes() -> (tempfile::TempDir, Coordinator) {
        let (dir, coordinator) = alone();
        lock(&coordinator.store)
            .apply_settings(SharedSettings {
                seq: 1,
                ..SharedSettings::default()
            })
            .unwrap();
        rusqlite::Connection::open(dir.path().join("foundry.db"))
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER refuse BEFORE UPDATE ON settings \
                 BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
            )
            .unwrap();
        (dir, coordinator)
    }

    #[tokio::test(start_paused = true)]
    async fn a_forwarded_edit_that_cannot_be_written_is_refused_when_nothing_else_waits_on_its_id()
    {
        let (_dir, coordinator) = refusing_writes();
        let error = coordinator
            .edit_from(
                Some(MachineId::random()),
                Instant::now(),
                EditId::random(),
                set_project(7, "p"),
            )
            .await
            .unwrap_err();
        let reason = format!("{} could not store it", coordinator.me);
        assert!(
            matches!(&error, CoordinatorError::Refused { reason: given } if *given == reason),
            "{error}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_forwarded_edit_that_cannot_be_written_is_not_refused_while_another_waits_on_its_id()
    {
        let (_dir, coordinator) = refusing_writes();
        let id = EditId::random();
        // Another task here may number the id, so a failed write says nothing about it.
        let _other = coordinator.wait_on(id);
        let error = coordinator
            .edit_from(
                Some(MachineId::random()),
                Instant::now(),
                id,
                set_project(7, "p"),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, CoordinatorError::Store(_)), "{error}");
    }

    #[test]
    fn a_store_error_is_never_answered_as_a_refusal() {
        let (_dir, coordinator) = alone();
        let error = CoordinatorError::Store(StoreError::TooLarge {
            what: "x",
            value: 1,
        });
        assert_eq!(coordinator.outcome(Err(error)), None);
    }
}
