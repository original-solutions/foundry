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
//! "setting overwritten" item, itself a setting, holding both values. One exception: a machine
//! removed from the machine list stays removed, so its removal holds over any later entry for
//! it, and a removed machine is never listed again ([`crate::join::is_removed`]).
//!
//! An edit is sent under an [`EditId`] its sender picks, and the acting machine answers it with
//! an [`Outcome`]: numbered, with its stamp, or refused. The sender keeps the edit until the
//! answer, or a copy of the settings that holds the id's stamp ([`remember`]), arrives, and
//! sends it again when the link or the role moves. An id is numbered once by any machine that
//! holds the copy that numbered it, so its sender learns what became of the edit; two sides of
//! a split that cannot reach each other can still number it once each.
//!
//! A machine reports an edit refused, or failed with a store error, only when no machine can
//! have numbered its id ([`may_refuse`], the one place that decides). Otherwise the outcome is
//! the stamp, or [`CoordinatorError::NoAnswer`] when [`KEEP_FORWARDED`] passes without it. So a
//! sender's refusal is final only when the edit went to one machine, whose own copy holds every
//! id it numbered; once an edit went to two, a refusal from either is ignored, as the other may
//! have numbered it. What a machine knows of an id ([`Known`]: who it sent it to, the answer)
//! outlasts the task that sent it by [`KEEP_FORWARDED`], so an edit sent again is judged on it.
//!
//! The edits one machine forwards are sent on, and numbered, in the order they arrived, also
//! when sent again ([`in_turn`]), so of two edits to one setting the later one holds.
//!
//! The acting machine checks the project rules as it numbers an edit, against its own copy and
//! store ([`breaks`]): it refuses a new project whose name another project has, and the removal
//! of a project its store holds a live workspace of, with [`CoordinatorError::Conflict`], which
//! [`may_refuse`] governs as any refusal. It refuses an entry for a machine its copy shows
//! removed the same way. Two sides of a split each number their own edits, so
//! neither check holds across a split. An edit with no id, from a machine before
//! [`ANSWERS_SINCE`], is numbered unchecked: nobody answers it, so a refusal would drop it unseen.
//!
//! [`Coordinator`] runs this on a machine. Record links hand it every copy of the settings a
//! peer sends and every edit forwarded to this machine ([`crate::sync::Peers::coordinate`]).
//! A machine that comes to act numbers nothing until it has taken a copy from every other
//! machine it can reach, except one linked only in a protocol that carries no settings, so a
//! returning machine catches up before it edits; an edit that reaches it before then waits,
//! at most [`OFFLINE_AFTER`].
//!
//! Factory settings ([`SettingKey::is_factory`]) are numbered like any setting, but a machine
//! before [`FACTORY_SINCE`] cannot read them. Links send such a peer the copy without them
//! ([`SharedSettings::for_protocol`]), and a copy it sends back leaves the ones held here as
//! they are ([`keep_factory`]). An edit of one is not sent to an acting machine whose link
//! speaks an older protocol: this machine's own fails with [`CoordinatorError::Older`], and a
//! forwarded one is refused, both naming the protocol.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use foundry_proto::ids::{MachineId, ProjectId};
use foundry_proto::messages::{ANSWERS_SINCE, FACTORY_SINCE};
use foundry_proto::record::{Record, Workspace};
use foundry_proto::settings::{
    Edit, EditId, Outcome, Overwritten, SettingKey, SettingValue, SharedSettings, Stamp, StampKey,
};
use serde::Serialize;
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

/// How far below the newest numbered edit an edit's id is kept in [`SharedSettings::numbered`],
/// counted in numbers, so every copy forgets the same ids. The machine that numbered an id, or
/// was answered its stamp, also keeps it for [`KEEP_FORWARDED`] ([`Known`]), the longest a sender
/// sends it again, so it answers a resend with the stamp however many numbers came since. The
/// window matters only when the role moves before the resend: then it must cover what the fleet
/// numbered in between.
const NUMBERED_KEPT: u64 = 128;

/// Why an edit was not made.
#[derive(Debug, thiserror::Error)]
pub enum CoordinatorError {
    #[error("no link to {0}, the acting coordinator, is open, so the edit was not sent")]
    NotLinked(MachineId),
    /// The acting machine's link speaks a protocol older than the edit needs: one before
    /// [`ANSWERS_SINCE`] answers no edit, and one before [`FACTORY_SINCE`] carries no factory
    /// setting, which is the only edit [`ANSWERS_SINCE`] or later refuses this way.
    #[error("{acting}, the acting coordinator, speaks protocol {speaks}, {}", older(*speaks))]
    Older { acting: MachineId, speaks: u16 },
    #[error("the edit was refused: {reason}")]
    Refused { reason: String },
    #[error("the edit breaks a project rule: {reason}")]
    Conflict { reason: String },
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

/// Why an acting machine that speaks `speaks` was not sent an edit, for
/// [`CoordinatorError::Older`].
fn older(speaks: u16) -> String {
    if speaks < ANSWERS_SINCE {
        "which does not answer edits, so the edit was not sent".to_owned()
    } else {
        format!(
            "which does not carry factory settings, so the edit was not sent; a factory \
             setting can be edited once the acting coordinator speaks protocol {FACTORY_SINCE}"
        )
    }
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
    /// The protocol of the link each machine's last copy of the settings came on. A machine is
    /// never heard from on a link that carries settings before its copy is taken, so this holds
    /// every reachable machine but one linked only before
    /// [`foundry_proto::messages::EDITS_SINCE`].
    speaks: Mutex<BTreeMap<MachineId, u16>>,
    /// When this machine started. Until every listed machine's copy is in, or
    /// [`OFFLINE_AFTER`] has passed, its own copy may be an old one.
    started: Instant,
    /// The forwarded edits with a task queued or running, so an edit sent again while its first
    /// copy waits is not made twice, and edits from one machine go in the order they arrived.
    queue: Mutex<Queue>,
    /// What this machine knows of each id it waits on, or waited on within [`KEEP_FORWARDED`].
    known: Mutex<BTreeMap<EditId, Known>>,
}

impl Coordinator {
    pub fn new(me: MachineId, store: Arc<Mutex<Store>>, peers: Peers) -> Self {
        Self {
            me,
            store,
            peers,
            taken: Arc::new(Mutex::new(BTreeMap::new())),
            speaks: Mutex::new(BTreeMap::new()),
            started: Instant::now(),
            queue: Mutex::new(Queue::default()),
            known: Mutex::new(BTreeMap::new()),
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
        self.run_edit(None, Instant::now(), EditId::random(), edit)
            .await
    }

    /// Takes the answer to the edit sent under `id`, when this machine knows the id ([`Known`]).
    /// An answer to an id it does not know is dropped.
    pub(crate) fn take_answer(&self, id: EditId, outcome: Outcome) {
        if let Some(known) = self.known().get_mut(&id) {
            // A stamp from any machine settles the edit, so it replaces a refusal.
            if known.answer.is_none() || matches!(outcome, Outcome::Numbered { .. }) {
                known.answer = Some(outcome);
            }
        }
    }

    /// Merges a copy of the settings `peer` sent into the held copy.
    ///
    /// A copy that came on a link in a protocol before [`ANSWERS_SINCE`] cannot carry the ids
    /// of numbered edits, so it is taken as holding the ones this copy holds: without that every
    /// merge with it would differ from both copies and raise the number again. A copy that came
    /// in a protocol before [`FACTORY_SINCE`] cannot carry factory settings, so it is taken as
    /// holding the ones held here, for the same reason ([`keep_factory`]).
    pub(crate) async fn take_settings(
        &self,
        peer: MachineId,
        speak: u16,
        mut settings: SharedSettings,
    ) -> Result<(), StoreError> {
        let (taken, at) = (Arc::clone(&self.taken), Instant::now());
        lock(&self.speaks).insert(peer, speak);
        on_store(&self.store, move |store| {
            let held = store.settings()?;
            if speak < ANSWERS_SINCE {
                settings.numbered = held.numbered.clone();
            }
            if speak < FACTORY_SINCE {
                keep_factory(&held, &mut settings);
            }
            keep_keys(&held, &mut settings);
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
    /// reading while the edit waits. The edits one machine forwards are sent on, and numbered,
    /// in the order they arrived, also when sent again ([`in_turn`]), and each one's waits count
    /// from when it arrived, not from its turn. When the edit came with an `id`, `peer` is
    /// answered under it: numbered, or refused with a fixed reason that names no setting value.
    /// A refusal is final for its id: an edit sent again under it is refused again, for
    /// [`KEEP_FORWARDED`]. An edit sent again by the same machine while its first copy is queued
    /// or running is dropped: that copy answers. An edit from a machine before [`ANSWERS_SINCE`]
    /// has no id: it is passed on as it came, or numbered here, and never answered.
    pub(crate) fn take_edit(self: Arc<Self>, peer: MachineId, id: Option<EditId>, edit: Edit) {
        let arrived = Instant::now();
        let Some(queued) = Queued::new(&self, peer, id) else {
            return;
        };
        tokio::spawn(async move {
            let Some(id) = id else {
                if let Err(error) = self.pass_on(&queued, arrived, edit).await {
                    tracing::warn!("coordinator: {error}");
                }
                return;
            };
            let outcome = match self.refusal(id) {
                Some(refused) => Some(refused),
                None => {
                    let result = self.run_edit(Some(&queued), arrived, id, edit).await;
                    if let Err(error) = &result {
                        tracing::warn!("coordinator: {error}");
                    }
                    outcome(result)
                }
            };
            if let Some(refused @ Outcome::Refused { .. }) = &outcome {
                self.remember_refusal(id, refused.clone());
            }
            // Only now may the next edit from `peer` go on: a copy of this id sees the refusal.
            drop(queued);
            if let Some(outcome) = outcome
                && !self.peers.send_answer(peer, id, outcome).await
            {
                tracing::warn!(
                    "coordinator: no link to {peer} took the answer to an edit it forwarded"
                );
            }
        });
    }

    /// What this machine knows of each id, without the ids it no longer keeps: those no task
    /// here waits on, whose last task ended [`KEEP_FORWARDED`] ago or more. A timeout, not an
    /// order of records, so the clock is fine here.
    fn known(&self) -> std::sync::MutexGuard<'_, BTreeMap<EditId, Known>> {
        let mut known = lock(&self.known);
        known.retain(|_, known| known.kept());
        known
    }

    /// The refusal this machine gave `id` within [`KEEP_FORWARDED`], if it did.
    fn refusal(&self, id: EditId) -> Option<Outcome> {
        self.known()
            .get(&id)
            .and_then(|known| known.refused.clone())
    }

    /// Notes that this machine answered `id` with `refused`: a refusal is final for its id.
    fn remember_refusal(&self, id: EditId, refused: Outcome) {
        let mut known = self.known();
        let known = known.entry(id).or_default();
        known.refused = Some(refused);
        if known.waiting == 0 {
            known.ended.get_or_insert_with(Instant::now);
        }
    }

    /// Whether the edit sent under `id`, which `from` forwarded or this machine's own when
    /// `None`, may be reported refused for `cause`, by what this machine knows of the id now
    /// ([`may_refuse`]).
    fn may_refuse(&self, id: EditId, from: Option<MachineId>, cause: Cause) -> bool {
        self.known()
            .get(&id)
            .is_some_and(|known| may_refuse(known, from.is_some(), cause))
    }

    /// A refusal from this machine for `why`: a fixed phrase built from machine ids only, never
    /// a store error's text or a setting value.
    fn refused(&self, why: &str) -> CoordinatorError {
        CoordinatorError::Refused {
            reason: format!("{} {why}", self.me),
        }
    }

    /// A refusal from this machine of an edit that breaks a project rule, for `why`: a fixed
    /// phrase, as [`Coordinator::refused`] builds.
    fn conflict(&self, why: &str) -> CoordinatorError {
        CoordinatorError::Conflict {
            reason: format!("{} {why}", self.me),
        }
    }

    /// Makes `edit`, sent under `id`, which `from` forwarded, or this machine's own when `None`;
    /// `arrived` is when it reached this machine. See [`Coordinator::run_edit`].
    #[cfg(test)]
    async fn edit_from(
        self: &Arc<Self>,
        from: Option<MachineId>,
        arrived: Instant,
        id: EditId,
        edit: Edit,
    ) -> Result<Stamp, CoordinatorError> {
        let queued = from.map(|from| Queued::new(self, from, Some(id)).expect("not queued yet"));
        self.run_edit(queued.as_ref(), arrived, id, edit).await
    }

    /// Makes `edit`, sent under `id`, which a peer forwarded as `queued`, or this machine's own
    /// when `None`; `arrived` is when it reached this machine. Only the acting machine numbers
    /// it, once caught up or [`OFFLINE_AFTER`] after it arrived, and it numbers an `id` once. A
    /// forwarded edit is sent, or numbered, only in its turn ([`in_turn`]).
    ///
    /// Every [`STEP`] it looks again. An answer to `id` settles it, and so does a copy of the
    /// settings that holds a stamp for `id`. Otherwise, while another machine acts, the edit is
    /// sent to it, and sent again when the link it went on closes or another machine acts
    /// ([`Coordinator::look`] says where it goes).
    ///
    /// It reports a refusal only when [`may_refuse`] allows it, and a store error only on this
    /// machine's own edit before it was ever sent. Otherwise a refusal and any store error are
    /// logged and it waits on, and [`KEEP_FORWARDED`] after it arrived it ends as
    /// [`CoordinatorError::NoAnswer`], naming the machine it went to last.
    async fn run_edit(
        &self,
        queued: Option<&Queued>,
        arrived: Instant,
        id: EditId,
        edit: Edit,
    ) -> Result<Stamp, CoordinatorError> {
        let from = queued.map(|queued| queued.peer);
        let _waiting = self.wait_on(id);
        // The acting machine this edit was last sent to, and the number of the link it went on.
        let mut sent: Sent = None;
        // The last read error and the last write error, and whether a refusal was ignored,
        // logged once each: a read that goes right does not clear a write that keeps failing.
        let mut read_logged: Option<String> = None;
        let mut write_logged: Option<String> = None;
        let mut ignored = false;
        let mut broke_logged = false;
        loop {
            let answered = self.known().get(&id).and_then(|known| known.answer.clone());
            if let Some(Outcome::Numbered { stamp }) = answered {
                return Ok(stamp);
            }
            let look = self.look(from, arrived).await;
            // A stamp in the held copy settles the id, whatever refusal came for it.
            if let Ok(look) = &look
                && let Some(stamp) = look.held.numbered.get(&id)
            {
                return Ok(self.settled(id, *stamp));
            }
            if let Some(Outcome::Refused { reason, conflict }) = answered {
                if self.may_refuse(id, from, Cause::Answered) {
                    return Err(if conflict {
                        CoordinatorError::Conflict { reason }
                    } else {
                        CoordinatorError::Refused { reason }
                    });
                }
                if !ignored {
                    ignored = true;
                    tracing::warn!(
                        "coordinator: a refusal of an edit another machine may have numbered; \
                         waiting"
                    );
                }
            }
            // The error to report when the limit passes with nothing settled.
            let mut failed: Option<(StoreError, bool)> = None;
            match look {
                // A read that fails ends only this machine's own edit that was never sent.
                Err(error) if from.is_none() && sent.is_none() => return Err(error.into()),
                Err(error) => failed = Some((error, false)),
                Ok(look) => {
                    read_logged = None;
                    let (next, on) = match queued {
                        Some(queued) => queued.turn(look.next),
                        None => (look.next, None),
                    };
                    match next {
                        // A failed write here is a cause to refuse; a failed read says nothing
                        // of the id. Any other failure ends only this machine's own edit that
                        // was never sent; otherwise it is logged and tried again.
                        Next::Number => match self.number(Some(id), &edit).await {
                            Ok(stamp) => return Ok(self.settled(id, stamp)),
                            // A rule refusal follows the same rule as a failed write; this
                            // machine's own edit never sent fails with it.
                            Err(Unnumbered::Breaks(why)) => {
                                if self.may_refuse(id, from, Cause::Here)
                                    || (from.is_none() && sent.is_none())
                                {
                                    return Err(self.conflict(why));
                                }
                                if !broke_logged {
                                    broke_logged = true;
                                    tracing::warn!(
                                        "coordinator: an edit another machine may have numbered \
                                         breaks a project rule here; waiting"
                                    );
                                }
                            }
                            Err(Unnumbered::Store { error, write }) => {
                                if write && self.may_refuse(id, from, Cause::Here) {
                                    return Err(self.refused("could not store it"));
                                }
                                if from.is_none() && sent.is_none() {
                                    return Err(error.into());
                                }
                                failed = Some((error, write));
                            }
                        },
                        Next::Send(acting) => {
                            let on_its_way = sent.is_some_and(|(to, link)| {
                                to == acting && self.peers.is_open(acting, link)
                            });
                            if !on_its_way {
                                let uncarried = self.uncarried(acting, &edit);
                                let result = match uncarried {
                                    Some(speaks) => Err(Unsent::Older(speaks)),
                                    None => self.peers.send_edit(acting, Some(id), &edit, on).await,
                                };
                                match result {
                                    Ok(link) => {
                                        sent = Some((acting, link));
                                        self.sent_to(id, acting);
                                        if let Some(queued) = queued {
                                            queued.sent(acting, link);
                                        }
                                    }
                                    Err(Unsent::Unwritable(_))
                                        if self.may_refuse(id, from, Cause::Here) =>
                                    {
                                        return Err(
                                            self.refused("could not write it to pass it on")
                                        );
                                    }
                                    Err(Unsent::Unwritable(error)) => {
                                        return Err(CoordinatorError::Unwritable(error));
                                    }
                                    Err(Unsent::Older(speaks))
                                        if uncarried.is_some()
                                            && self.may_refuse(id, from, Cause::Here) =>
                                    {
                                        return Err(self.refused(&format!(
                                            "cannot pass a factory setting on to {acting}, the \
                                             acting coordinator, which speaks protocol {speaks}: \
                                             factory settings need protocol {FACTORY_SINCE}"
                                        )));
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
                            (None, Some(_)) if self.may_refuse(id, from, Cause::Here) => self
                                .refused(&format!(
                                    "could not send it to {}, the acting coordinator",
                                    look.acting
                                )),
                            (None, Some(from)) => CoordinatorError::Unsettled {
                                from,
                                acting: look.acting,
                            },
                            (None, None) => CoordinatorError::NotLinked(look.acting),
                        });
                    }
                }
            }
            if let Some((error, write)) = failed {
                let text = error.to_string();
                let logged = if write {
                    &mut write_logged
                } else {
                    &mut read_logged
                };
                if logged.as_ref() != Some(&text) {
                    tracing::warn!("coordinator: {text}; looking again");
                    *logged = Some(text);
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
    /// acts. This is the one place that decides
    /// where an edit goes, for an edit with an id ([`Coordinator::run_edit`]) and one without
    /// ([`Coordinator::pass_on`]).
    async fn look(&self, from: Option<MachineId>, arrived: Instant) -> Result<Look, StoreError> {
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
        Ok(Look {
            held,
            next,
            acting,
            waited,
        })
    }

    /// Passes on, or numbers here, an edit a peer forwarded with no id, as `queued`, because it
    /// came from a machine before [`ANSWERS_SINCE`]. Nobody answers it, so this ends once the
    /// edit is sent on a link that speaks [`foundry_proto::messages::EDITS_SINCE`] or later, or
    /// numbered, and never sends it again. It differs from [`Coordinator::run_edit`] only in
    /// that: it has no id to look up or be answered under, so nothing here can be refused.
    async fn pass_on(
        &self,
        queued: &Queued,
        arrived: Instant,
        edit: Edit,
    ) -> Result<(), CoordinatorError> {
        let peer = queued.peer;
        let mut logged: Option<String> = None;
        loop {
            match self.look(Some(peer), arrived).await {
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
                    logged = None;
                    let (next, on) = queued.turn(look.next);
                    match next {
                        Next::Number => {
                            return match self.number(None, &edit).await {
                                Ok(_) => Ok(()),
                                Err(Unnumbered::Store { error, .. }) => Err(error.into()),
                                Err(Unnumbered::Breaks(_)) => {
                                    unreachable!("an edit with no id is numbered unchecked")
                                }
                            };
                        }
                        Next::Send(acting) => {
                            match self.peers.send_edit(acting, None, &edit, on).await {
                                Ok(link) => {
                                    queued.sent(acting, link);
                                    return Ok(());
                                }
                                Err(Unsent::Unwritable(error)) => {
                                    return Err(CoordinatorError::Unwritable(error));
                                }
                                Err(Unsent::NotLinked | Unsent::Older(_)) => {}
                            }
                        }
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
    /// on one id are counted, and what this machine knows of it is kept until
    /// [`KEEP_FORWARDED`] after the last of them is done.
    fn wait_on(&self, id: EditId) -> Waiting<'_> {
        let mut known = self.known();
        let known = known.entry(id).or_default();
        known.waiting += 1;
        known.ended = None;
        Waiting {
            known: &self.known,
            id,
        }
    }

    /// Notes that the edit under `id` was numbered with `stamp`, as this machine found in its own
    /// copy or numbered itself, and returns the stamp. Like a stamp answered, it replaces any
    /// refusal held for the id, and it answers the id while it is kept, whatever the shared
    /// copy has forgotten since ([`NUMBERED_KEPT`]).
    fn settled(&self, id: EditId, stamp: Stamp) -> Stamp {
        if let Some(known) = self.known().get_mut(&id) {
            known.answer = Some(Outcome::Numbered { stamp });
        }
        stamp
    }

    /// Notes that a task here sent the edit under `id` to `acting`.
    fn sent_to(&self, id: EditId, acting: MachineId) {
        if let Some(known) = self.known().get_mut(&id) {
            known.sent_to.insert(acting);
        }
    }

    /// Gives the edit the next number, on the copy held at that moment, unless that copy
    /// already holds a stamp for its `id`, which is returned. An edit with an id that breaks a
    /// project rule in that copy and store ([`breaks`]) is not numbered; one with none is not
    /// checked.
    async fn number(&self, id: Option<EditId>, edit: &Edit) -> Result<Stamp, Unnumbered> {
        let me = self.me;
        let edit = edit.clone();
        on_store(&self.store, move |store| {
            let held = store.settings().map_err(Unnumbered::read)?;
            if let Some(stamp) = id.and_then(|id| held.numbered.get(&id)) {
                return Ok(*stamp);
            }
            if id.is_some()
                && let Some(why) = breaks(store, &held, &edit).map_err(Unnumbered::read)?
            {
                return Err(Unnumbered::Breaks(why));
            }
            let mut next = numbered(&held, &edit, me);
            let stamp = Stamp {
                seq: next.seq,
                by: me,
            };
            if let Some(id) = id {
                remember(&mut next, id, stamp);
            }
            store.apply_settings(next).map_err(Unnumbered::write)?;
            Ok(stamp)
        })
        .await
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

    /// The protocol `acting` speaks when it is older than [`FACTORY_SINCE`] and `edit` changes a
    /// factory setting, which such a machine cannot read: the edit is not sent to it. `None`
    /// when the edit may go, also when this machine has taken no copy from `acting` yet, as
    /// [`crate::sync::Peers::send_edit`] then says why it cannot.
    fn uncarried(&self, acting: MachineId, edit: &Edit) -> Option<u16> {
        if !edit.key().setting().is_factory() {
            return None;
        }
        lock(&self.speaks)
            .get(&acting)
            .copied()
            .filter(|speaks| *speaks < FACTORY_SINCE)
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

/// An edit this machine waits on, counted in [`Coordinator::known`] until this drops.
struct Waiting<'a> {
    known: &'a Mutex<BTreeMap<EditId, Known>>,
    id: EditId,
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        if let Some(known) = lock(self.known).get_mut(&self.id) {
            known.waiting -= 1;
            if known.waiting == 0 {
                known.ended = Some(Instant::now());
            }
        }
    }
}

/// What this machine knows of one edit id, in [`Coordinator::known`].
#[derive(Debug, Default)]
struct Known {
    /// How many tasks here wait on it.
    waiting: usize,
    /// When the last task that waited on it ended, `None` while one waits. It is kept for
    /// [`KEEP_FORWARDED`] after that: the longest its sender sends it again.
    ended: Option<Instant>,
    /// Its stamp, once this machine numbered it, found it in its copy or was answered it; else
    /// the refusal a machine it was sent to answered.
    answer: Option<Outcome>,
    /// Every machine a task here sent it to.
    sent_to: BTreeSet<MachineId>,
    /// The refusal this machine answered it with: final for the id.
    refused: Option<Outcome>,
}

impl Known {
    fn kept(&self) -> bool {
        self.waiting > 0 || self.ended.is_some_and(|at| at.elapsed() < KEEP_FORWARDED)
    }
}

/// What may end an edit as refused.
#[derive(Clone, Copy, Debug)]
enum Cause {
    /// A machine it was sent to answered it refused.
    Answered,
    /// This machine failed it: could not store it while acting, found it breaks a project rule
    /// while acting, could not write it to send it on, or send it on within [`KEEP_FORWARDED`].
    Here,
}

/// The one rule for refusals: an edit is reported refused for `cause` only when no machine can
/// have numbered its id, by what this machine knows of it, `known`, while a task here waits on
/// it. `forwarded` is whether a peer forwarded the edit: this machine's own edit is not refused
/// for a failure here, it fails with the error.
fn may_refuse(known: &Known, forwarded: bool, cause: Cause) -> bool {
    match cause {
        // The refusing machine's own copy holds every id it numbered, so its refusal is final
        // while no other machine was sent the id.
        Cause::Answered => known.sent_to.len() <= 1,
        // Once a task here sent the id on, the machine it went to may have numbered it, and
        // another task here that waits on it may be numbering it now.
        Cause::Here => forwarded && known.sent_to.is_empty() && known.waiting <= 1,
    }
}

/// Where an edit was last sent: the acting machine and the number of the link, `None` before
/// it was sent.
type Sent = Option<(MachineId, u64)>;

/// The forwarded edits with a task queued or running, in [`Coordinator::queue`].
#[derive(Default)]
struct Queue {
    /// The number the next edit to arrive takes.
    next: u64,
    /// Each edit by the machine that forwarded it and its number: its id, and the machine and
    /// link it was last sent on.
    edits: BTreeMap<(MachineId, u64), (Option<EditId>, Sent)>,
    /// The machine and link each machine's forwarded edit went on last. An edit with no id
    /// leaves [`Queue::edits`] once sent, as it is never sent again, and may still wait unread
    /// on that link.
    last: BTreeMap<MachineId, (MachineId, u64)>,
}

/// An edit `peer` forwarded with a task queued or running, in [`Coordinator::queue`], removed
/// when this drops.
struct Queued {
    coordinator: Arc<Coordinator>,
    peer: MachineId,
    number: u64,
}

impl Queued {
    /// Queues an edit `peer` forwarded under `id`; `None` when one from `peer` under that id is
    /// queued already.
    fn new(coordinator: &Arc<Coordinator>, peer: MachineId, id: Option<EditId>) -> Option<Self> {
        let mut queue = lock(&coordinator.queue);
        let mine = (peer, 0)..=(peer, u64::MAX);
        if id.is_some() && queue.edits.range(mine).any(|(_, (held, _))| *held == id) {
            return None;
        }
        let number = queue.next;
        queue.next += 1;
        queue.edits.insert((peer, number), (id, None));
        Some(Self {
            coordinator: Arc::clone(coordinator),
            peer,
            number,
        })
    }

    /// Notes that the edit went to `acting` on link `link`.
    fn sent(&self, acting: MachineId, link: u64) {
        let mut queue = lock(&self.coordinator.queue);
        if let Some((_, sent)) = queue.edits.get_mut(&(self.peer, self.number)) {
            *sent = Some((acting, link));
        }
        queue.last.insert(self.peer, (acting, link));
    }

    /// What the edit may do now, for `next`, and the link it must go on ([`in_turn`]).
    fn turn(&self, next: Next) -> Turn {
        let (earlier, last): (Vec<Sent>, Sent) = {
            let queue = lock(&self.coordinator.queue);
            let earlier = queue
                .edits
                .range((self.peer, 0)..(self.peer, self.number))
                .map(|(_, (_, sent))| *sent)
                .collect();
            (earlier, queue.last.get(&self.peer).copied())
        };
        let peers = &self.coordinator.peers;
        let last = last.filter(|(machine, link)| peers.is_open(*machine, *link));
        in_turn(next, &earlier, last)
    }
}

impl Drop for Queued {
    fn drop(&mut self) {
        lock(&self.coordinator.queue)
            .edits
            .remove(&(self.peer, self.number));
    }
}

/// What a forwarded edit does next, and the number of the link it must go on, `None` for any.
type Turn = (Next, Option<u64>);

/// What a forwarded edit may do now, for `next` from its look, given where each edit the same
/// machine forwarded before it, still queued or running here, was last sent: `earlier`. It is
/// numbered only once none of them is left. It is sent only to the machine they all went to, on
/// the one link they all went on, which the second value names; when that link has closed, it
/// waits until they are sent again. With none earlier it goes on `last`, the open link that
/// machine's edit went on last, when that goes to the same machine, and else on any link
/// (`None`). Otherwise it waits. Links carry messages in order, so edits from one machine reach
/// the acting machine, and take their numbers, in the order they arrived, also when sent again.
fn in_turn(next: Next, earlier: &[Sent], last: Sent) -> Turn {
    match (next, earlier.first()) {
        (Next::Number, None) => (next, None),
        (Next::Send(acting), None) => (
            next,
            last.filter(|(to, _)| *to == acting).map(|(_, link)| link),
        ),
        (Next::Send(acting), Some(&Some((to, link))))
            if to == acting && earlier.iter().all(|sent| *sent == Some((acting, link))) =>
        {
            (next, Some(link))
        }
        _ => (Next::Wait, None),
    }
}

/// What to tell a machine whose forwarded edit ended in `result`: its stamp, or the refusal
/// [`Coordinator::run_edit`] gave under [`may_refuse`]. Any other end leaves open whether the
/// edit was numbered, so nothing is told and the forwarder's own limit reports it.
fn outcome(result: Result<Stamp, CoordinatorError>) -> Option<Outcome> {
    match result {
        Ok(stamp) => Some(Outcome::Numbered { stamp }),
        Err(CoordinatorError::Refused { reason }) => Some(Outcome::Refused {
            reason,
            conflict: false,
        }),
        Err(CoordinatorError::Conflict { reason }) => Some(Outcome::Refused {
            reason,
            conflict: true,
        }),
        Err(_) => None,
    }
}

/// Why [`Coordinator::number`] stored no stamp.
enum Unnumbered {
    /// The store's error, and whether it came from the write, after the read found no stamp
    /// for the id, or from the read.
    Store { error: StoreError, write: bool },
    /// The edit breaks a project rule, for this fixed phrase ([`breaks`]).
    Breaks(&'static str),
}

impl Unnumbered {
    fn read(error: StoreError) -> Self {
        Self::Store {
            error,
            write: false,
        }
    }

    fn write(error: StoreError) -> Self {
        Self::Store { error, write: true }
    }
}

/// The project rule `edit` breaks in `held` and the workspaces `store` holds, as a fixed phrase
/// that names no setting value, or `None`. A project for an id `held` does not hold breaks it
/// when another project has its name, compared trimmed and in any case. Removing a project
/// breaks it while a live workspace in `store`, from any machine, was made from it. An entry
/// for a machine `held` shows removed breaks the rule that a removal is one-way.
fn breaks(
    store: &Store,
    held: &SharedSettings,
    edit: &Edit,
) -> Result<Option<&'static str>, StoreError> {
    Ok(match edit {
        Edit::SetProject { project } if !held.projects.iter().any(|p| p.id == project.id) => held
            .project_named(&project.name)
            .map(|_| "holds another project with that name"),
        Edit::RemoveProject { id } => (!holding(store, *id)?.is_empty())
            .then_some("holds a live workspace made from that project"),
        Edit::SetMachine { machine } if crate::join::is_removed(held, machine.id) => {
            Some("holds that machine removed from the machine list")
        }
        _ => None,
    })
}

/// The live workspaces in `store`, from any machine, made from `project`: the one rule for
/// which workspaces keep a project from being removed. The route that serves a remove and the
/// acting machine's check ([`breaks`]) both read it.
pub fn holding(store: &Store, project: ProjectId) -> Result<Vec<Record<Workspace>>, StoreError> {
    let mut live = store.live_workspaces()?;
    live.retain(|w| w.body.project == project);
    Ok(live)
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

/// Puts back in `copy` each machine's key that `held` has for the same edit, the same stamp,
/// where `copy` has none: a machine from before keys were listed drops the key when it reads
/// an entry and sends the edit on without it. One edit has one value, so the key is its own;
/// without this the copy without it would differ from the held one at the same stamp, and
/// every merge with that machine would take a new number.
pub fn keep_keys(held: &SharedSettings, copy: &mut SharedSettings) {
    let stamps = copy.stamps.clone();
    for machine in copy.machines.iter_mut().filter(|m| m.key.is_empty()) {
        let key = StampKey::Setting(SettingKey::Machine(machine.id));
        let theirs = stamps.get(&key).copied().unwrap_or(Stamp::NONE);
        if stamp(held, &key) != theirs {
            continue;
        }
        if let Some(kept) = held.machines.iter().find(|m| m.id == machine.id) {
            machine.key.clone_from(&kept.key);
        }
    }
}

/// Puts in `copy` the factory settings `held` holds, with their stamps and their "setting
/// overwritten" items, in place of any `copy` has: `copy` came from a machine before
/// [`FACTORY_SINCE`], which is sent none and cannot hold one ([`SharedSettings::for_protocol`]).
/// Without this the copy would differ from the held one at the same `seq`, and every merge
/// with that machine would take a new number.
pub fn keep_factory(held: &SharedSettings, copy: &mut SharedSettings) {
    copy.factory.clone_from(&held.factory);
    copy.project_factory.clone_from(&held.project_factory);
    copy.stamps.retain(|key, _| !key.setting().is_factory());
    copy.stamps.extend(
        held.stamps
            .iter()
            .filter(|(key, _)| key.setting().is_factory())
            .map(|(key, stamp)| (key.clone(), *stamp)),
    );
    copy.overwritten.retain(|key, _| !key.is_factory());
    copy.overwritten.extend(
        held.overwritten
            .iter()
            .filter(|(key, _)| key.is_factory())
            .map(|(key, item)| (key.clone(), item.clone())),
    );
}

/// One copy of the settings from two, setting by setting: the higher stamp's value holds, except
/// that a machine either copy shows removed stays removed ([`crate::join::is_removed`]), at the
/// higher of the two removal stamps when both do. A setting both copies edited where neither
/// had seen the other's edit also gets a "setting overwritten" item holding both values,
/// stamped as the edit that held, or as the dismissal of an earlier item about that setting
/// when that is later. `seen` is the higher of each.
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
        let (ra, rb) = match key {
            StampKey::Setting(SettingKey::Machine(id)) => (
                crate::join::is_removed(a, *id),
                crate::join::is_removed(b, *id),
            ),
            _ => (false, false),
        };
        let a_holds = match (ra, rb, sa.cmp(&sb)) {
            // A removal is one-way: it holds over an entry, whatever the entry's stamp.
            (true, false, _) => true,
            (false, true, _) => false,
            (_, _, std::cmp::Ordering::Greater) => true,
            (_, _, std::cmp::Ordering::Less) => false,
            // One edit, or two copies no edit has stamped: pick by the value, the same way on
            // every machine.
            (_, _, std::cmp::Ordering::Equal) => rank(&va) >= rank(&vb),
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
    // Every field is named, so a new kind of setting does not compile until it is listed here.
    let SharedSettings {
        seq: _,
        machines,
        priority,
        projects,
        limits,
        stamps,
        seen: _,
        overwritten,
        numbered: _,
        factory,
        project_factory,
    } = settings;
    let mut keys: BTreeSet<StampKey> = stamps.keys().cloned().collect();
    let mut plain: Vec<SettingKey> = Vec::new();
    plain.extend(machines.iter().map(|m| SettingKey::Machine(m.id)));
    plain.extend(projects.iter().map(|p| SettingKey::Project(p.id)));
    plain.extend(limits.keys().map(|id| SettingKey::Limits(*id)));
    if !priority.is_empty() {
        plain.push(SettingKey::Priority);
    }
    if factory.is_some() {
        plain.push(SettingKey::Factory);
    }
    plain.extend(
        project_factory
            .keys()
            .map(|id| SettingKey::ProjectFactory(*id)),
    );
    keys.extend(plain.into_iter().map(StampKey::Setting));
    keys.extend(overwritten.keys().cloned().map(StampKey::Overwritten));
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
        SettingKey::Factory => settings.factory.clone().map(SettingValue::Factory),
        SettingKey::ProjectFactory(id) => settings
            .project_factory
            .get(id)
            .cloned()
            .map(SettingValue::ProjectFactory),
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
        StampKey::Setting(SettingKey::Factory) => {
            settings.factory = match value {
                Some(Value::Setting(SettingValue::Factory(factory))) => Some(factory),
                _ => None,
            };
        }
        StampKey::Setting(SettingKey::ProjectFactory(id)) => match value {
            Some(Value::Setting(SettingValue::ProjectFactory(factory))) => {
                settings.project_factory.insert(*id, factory);
            }
            _ => {
                settings.project_factory.remove(id);
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
    fn alone() -> (tempfile::TempDir, Arc<Coordinator>) {
        let dir = tempfile::tempdir().unwrap();
        let me = MachineId::random();
        let store = Arc::new(Mutex::new(
            Store::open(dir.path().join("foundry.db")).unwrap(),
        ));
        let peers = Peers::new(me, store.clone(), Arc::new(NoDial));
        (dir, Arc::new(Coordinator::new(me, store, peers)))
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
                key: String::new(),
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

    #[test]
    fn a_factory_edit_is_numbered_and_held_like_any_setting() {
        use foundry_proto::factory::{FactorySettings, ProjectFactory};

        let fleet = FactorySettings {
            max_runs: 5,
            ..FactorySettings::default()
        };
        let one = ProjectFactory {
            on: true,
            login: "rawphp".into(),
            paused: false,
        };
        let project = ProjectId(uuid::Uuid::from_u128(7));
        let held = numbered(
            &base(),
            &Edit::SetFactory {
                settings: fleet.clone(),
            },
            id(A),
        );
        let held = numbered(
            &held,
            &Edit::SetProjectFactory {
                project,
                factory: one.clone(),
            },
            id(A),
        );
        assert_eq!(held.factory, Some(fleet));
        assert_eq!(held.project_factory.get(&project), Some(&one));
        assert_eq!(
            held.stamps[&StampKey::Setting(SettingKey::Factory)],
            Stamp { seq: 2, by: id(A) }
        );
        assert_eq!(
            held.stamps[&StampKey::Setting(SettingKey::ProjectFactory(project))],
            Stamp { seq: 3, by: id(A) }
        );
        // Each is a setting of its own: the other settings are as they were.
        assert_eq!(held.machines, base().machines);
        assert_eq!(held.priority, base().priority);
    }

    #[test]
    fn a_factory_setting_survives_a_merge_and_a_split_over_it_is_overwritten() {
        use foundry_proto::factory::{FactorySettings, ProjectFactory};

        let project = ProjectId(uuid::Uuid::from_u128(7));
        let fleet = |max_runs| FactorySettings {
            max_runs,
            ..FactorySettings::default()
        };
        let one = ProjectFactory {
            on: true,
            login: "rawphp".into(),
            paused: false,
        };
        let set = numbered(&base(), &Edit::SetFactory { settings: fleet(5) }, id(A));
        let set = numbered(
            &set,
            &Edit::SetProjectFactory {
                project,
                factory: one.clone(),
            },
            id(A),
        );
        // A copy from a machine that has not seen the edits leaves them as they are.
        let kept = merged(&set, &base());
        assert_eq!(kept.factory, Some(fleet(5)));
        assert_eq!(kept.project_factory.get(&project), Some(&one));
        assert_eq!(kept.stamps, set.stamps);
        assert!(kept.overwritten.is_empty(), "{:?}", kept.overwritten);

        // Both sides of a split set the fleet's factory settings: one holds, and the other is
        // kept in a "setting overwritten" item.
        let a = numbered(&base(), &Edit::SetFactory { settings: fleet(4) }, id(A));
        let b = numbered(&base(), &Edit::SetFactory { settings: fleet(6) }, id(B));
        let split = merged(&a, &b);
        let item = &split.overwritten[&SettingKey::Factory];
        let kept = split.factory.clone().expect("one side holds");
        assert_eq!(item.kept, Some(SettingValue::Factory(kept.clone())));
        let lost = if kept == fleet(4) { fleet(6) } else { fleet(4) };
        assert_eq!(item.lost, Some(SettingValue::Factory(lost)));
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
            key: String::new(),
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
    fn a_copy_that_dropped_a_machines_key_does_not_take_it_from_the_same_edit() {
        let c = Machine {
            id: id(9),
            name: "c".into(),
            address: "100.64.0.9".into(),
            key: "ab".repeat(32),
        };
        let keyed = numbered(&base(), &Edit::SetMachine { machine: c.clone() }, id(A));
        // As a machine from before keys were listed holds the same edit.
        let mut stripped = keyed.clone();
        for m in &mut stripped.machines {
            m.key.clear();
        }

        keep_keys(&keyed, &mut stripped);

        assert!(stripped.machines.contains(&c), "{:?}", stripped.machines);
        assert_eq!(
            merged(&keyed, &stripped),
            keyed,
            "the merge takes no new number"
        );
    }

    #[tokio::test]
    async fn a_copy_from_a_machine_that_drops_keys_changes_nothing_held() {
        let (_dir, coordinator) = alone();
        let c = Machine {
            id: id(9),
            name: "c".into(),
            address: "100.64.0.9".into(),
            key: "ab".repeat(32),
        };
        let keyed = numbered(&base(), &Edit::SetMachine { machine: c }, id(A));
        lock(&coordinator.store)
            .apply_settings(keyed.clone())
            .unwrap();
        let mut stripped = keyed.clone();
        for m in &mut stripped.machines {
            m.key.clear();
        }

        coordinator
            .take_settings(id(B), foundry_proto::messages::PROTOCOL, stripped)
            .await
            .unwrap();

        assert_eq!(lock(&coordinator.store).settings().unwrap(), keyed);
    }

    #[test]
    fn a_copy_without_a_key_from_another_edit_keeps_its_own_value() {
        let c = Machine {
            id: id(9),
            name: "c".into(),
            address: "100.64.0.9".into(),
            key: "ab".repeat(32),
        };
        let keyed = numbered(&base(), &Edit::SetMachine { machine: c.clone() }, id(A));
        let renamed = Machine {
            name: "renamed".into(),
            key: String::new(),
            ..c
        };
        let mut later = numbered(
            &keyed,
            &Edit::SetMachine {
                machine: renamed.clone(),
            },
            id(B),
        );

        keep_keys(&keyed, &mut later);

        assert!(later.machines.contains(&renamed), "{:?}", later.machines);
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

    fn listed_machine(n: u128, address: &str) -> Machine {
        Machine {
            id: id(n),
            name: format!("m{n}"),
            address: address.into(),
            key: String::new(),
        }
    }

    fn lists(settings: &SharedSettings, n: u128) -> bool {
        settings.machines.iter().any(|m| m.id == id(n))
    }

    #[test]
    fn a_machine_removed_on_one_side_stays_removed_though_it_listed_itself_later_on_the_other() {
        const C: u128 = 9;
        let key = StampKey::Setting(SettingKey::Machine(id(C)));
        let synced = numbered(
            &base(),
            &Edit::SetMachine {
                machine: listed_machine(C, "100.64.0.9"),
            },
            id(A),
        );
        let removed = numbered(&synced, &Edit::RemoveMachine { id: id(C) }, id(A));
        // C missed the removal and, acting alone, lists itself again at new addresses: its
        // stamp is the higher one.
        let mut on_c = synced.clone();
        for n in 0..3 {
            let machine = listed_machine(C, &format!("100.64.1.{n}"));
            on_c = numbered(&on_c, &Edit::SetMachine { machine }, id(C));
        }
        assert!(stamp(&on_c, &key) > stamp(&removed, &key));

        let out = merged(&removed, &on_c);

        assert!(!lists(&out, C), "the removed machine is listed again");
        assert!(crate::join::is_removed(&out, id(C)));
    }

    #[test]
    fn a_machine_that_missed_a_removal_and_took_the_removed_machines_copy_drops_it_on_meeting_the_removal()
     {
        const C: u128 = 9;
        let synced = numbered(
            &base(),
            &Edit::SetMachine {
                machine: listed_machine(C, "100.64.0.9"),
            },
            id(A),
        );
        // A and B hold the removal. C missed it and lists itself again; D, offline during the
        // removal, links with C first and takes C's copy.
        let removed = numbered(&synced, &Edit::RemoveMachine { id: id(C) }, id(A));
        let mut on_c = synced.clone();
        for n in 0..2 {
            let machine = listed_machine(C, &format!("100.64.1.{n}"));
            on_c = numbered(&on_c, &Edit::SetMachine { machine }, id(C));
        }
        let on_d = merged(&synced, &on_c);
        assert!(lists(&on_d, C), "D lists C until it hears of the removal");

        // D meets a machine that holds the removal: C is removed on both, in either order, and
        // merging every copy in any grouping gives the same result.
        let out = merged(&on_d, &removed);
        assert!(!lists(&out, C) && crate::join::is_removed(&out, id(C)));
        let other_grouping = merged(&merged(&removed, &on_c), &on_d);
        assert_eq!(out.machines, other_grouping.machines);
        assert_eq!(out.stamps, other_grouping.stamps);
    }

    #[test]
    fn two_removals_of_one_machine_keep_the_higher_stamp() {
        const C: u128 = 9;
        let key = StampKey::Setting(SettingKey::Machine(id(C)));
        let synced = numbered(
            &base(),
            &Edit::SetMachine {
                machine: listed_machine(C, "100.64.0.9"),
            },
            id(A),
        );
        let on_a = numbered(&synced, &Edit::RemoveMachine { id: id(C) }, id(A));
        let on_b = numbered(
            &numbered(&synced, &Edit::RemoveLimits { machine: id(B) }, id(B)),
            &Edit::RemoveMachine { id: id(C) },
            id(B),
        );

        let out = merged(&on_a, &on_b);

        assert!(!lists(&out, C));
        assert_eq!(
            stamp(&out, &key),
            stamp(&on_a, &key).max(stamp(&on_b, &key))
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
    fn a_factory_setting_with_no_stamp_is_kept_by_a_merge() {
        use foundry_proto::factory::{FactorySettings, ProjectFactory};

        let project = ProjectId(uuid::Uuid::from_u128(7));
        let fleet = FactorySettings {
            max_runs: 5,
            ..FactorySettings::default()
        };
        let one = ProjectFactory {
            on: true,
            login: "rawphp".into(),
            paused: false,
        };
        let mut a = base();
        a.factory = Some(fleet.clone());
        a.project_factory.insert(project, one.clone());
        let out = merged(&a, &base());
        assert_eq!(out.factory, Some(fleet));
        assert_eq!(out.project_factory.get(&project), Some(&one));
        assert!(out.overwritten.is_empty(), "{:?}", out.overwritten);
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
            conflict: false,
        };
        coordinator.take_answer(id, outcome.clone());
        let held = coordinator
            .known()
            .get(&id)
            .and_then(|known| known.answer.clone());
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
    fn refusing_writes() -> (tempfile::TempDir, Arc<Coordinator>) {
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
    fn only_a_stamp_or_a_refusal_is_answered_and_every_other_end_is_not() {
        let acting = MachineId::random();
        let stamp = Stamp { seq: 3, by: acting };
        assert_eq!(outcome(Ok(stamp)), Some(Outcome::Numbered { stamp }));
        let refused = CoordinatorError::Refused {
            reason: "no".into(),
        };
        assert_eq!(
            outcome(Err(refused)),
            Some(Outcome::Refused {
                reason: "no".into(),
                conflict: false,
            })
        );
        let broken = CoordinatorError::Conflict {
            reason: "no".into(),
        };
        assert_eq!(
            outcome(Err(broken)),
            Some(Outcome::Refused {
                reason: "no".into(),
                conflict: true,
            })
        );
        // Each of these leaves open whether the edit was numbered.
        let unwritable = serde_json::from_str::<u8>("x").unwrap_err();
        for error in [
            CoordinatorError::Store(StoreError::TooLarge {
                what: "x",
                value: 1,
            }),
            CoordinatorError::NoAnswer { acting },
            CoordinatorError::NotLinked(acting),
            CoordinatorError::Older { acting, speaks: 3 },
            CoordinatorError::Unwritable(unwritable),
            CoordinatorError::Unsettled {
                from: MachineId::random(),
                acting,
            },
        ] {
            let text = error.to_string();
            assert_eq!(outcome(Err(error)), None, "{text}");
        }
    }

    #[test]
    fn an_edit_is_refused_only_when_no_machine_can_have_numbered_it() {
        let (a, b) = (MachineId::random(), MachineId::random());
        let known = |waiting: usize, sent_to: &[MachineId]| Known {
            waiting,
            sent_to: sent_to.iter().copied().collect(),
            ..Known::default()
        };
        let rows = [
            // (what is known, forwarded, cause, may refuse)
            (known(1, &[a]), true, Cause::Answered, true),
            (known(1, &[a]), false, Cause::Answered, true),
            (known(2, &[a]), true, Cause::Answered, true),
            (known(1, &[a, b]), true, Cause::Answered, false),
            (known(1, &[a, b]), false, Cause::Answered, false),
            (known(1, &[]), true, Cause::Here, true),
            // Its own edit fails with the error instead.
            (known(1, &[]), false, Cause::Here, false),
            // A machine it went to may have numbered it.
            (known(1, &[a]), true, Cause::Here, false),
            // Another task here may be numbering it.
            (known(2, &[]), true, Cause::Here, false),
        ];
        for (known, forwarded, cause, want) in rows {
            assert_eq!(
                may_refuse(&known, forwarded, cause),
                want,
                "{known:?} forwarded {forwarded} {cause:?}"
            );
        }
    }

    #[test]
    fn a_forwarded_edit_goes_only_after_the_ones_its_machine_forwarded_before_it() {
        let (a, b) = (MachineId::random(), MachineId::random());
        let wait = (Next::Wait, None);
        let rows: [(Next, &[Sent], Sent, Turn); 13] = [
            (Next::Number, &[], None, (Next::Number, None)),
            (Next::Number, &[], Some((a, 3)), (Next::Number, None)),
            (Next::Number, &[Some((a, 1))], None, wait),
            (Next::Send(a), &[], None, (Next::Send(a), None)),
            // With none earlier, on the open link the last edit went on, to the same machine.
            (Next::Send(a), &[], Some((a, 3)), (Next::Send(a), Some(3))),
            (Next::Send(a), &[], Some((b, 3)), (Next::Send(a), None)),
            // On the link the earlier ones went on, whether or not it is still open.
            (
                Next::Send(a),
                &[Some((a, 1))],
                Some((a, 3)),
                (Next::Send(a), Some(1)),
            ),
            (
                Next::Send(a),
                &[Some((a, 1)), Some((a, 1))],
                None,
                (Next::Send(a), Some(1)),
            ),
            // An earlier one not sent yet, sent elsewhere, or on another link goes first.
            (Next::Send(a), &[None], None, wait),
            (Next::Send(a), &[Some((b, 1))], None, wait),
            (Next::Send(a), &[Some((a, 1)), Some((a, 2))], None, wait),
            (Next::Send(a), &[Some((a, 1)), None], None, wait),
            (Next::Wait, &[None], None, wait),
        ];
        for (next, earlier, last, want) in rows {
            assert_eq!(
                in_turn(next, earlier, last),
                want,
                "{next:?} {earlier:?} {last:?}"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_forwarded_edit_is_numbered_only_once_the_edit_its_machine_sent_before_is_done() {
        let (_dir, coordinator) = alone();
        let peer = MachineId::random();
        let earlier = Queued::new(&coordinator, peer, Some(EditId::random())).unwrap();
        // Another machine's edits are not held back by it.
        let other = coordinator
            .edit_from(
                Some(MachineId::random()),
                Instant::now(),
                EditId::random(),
                set_project(8, "elsewhere"),
            )
            .await;
        assert!(other.is_ok(), "{other:?}");

        let later = tokio::spawn({
            let coordinator = Arc::clone(&coordinator);
            async move {
                coordinator
                    .edit_from(
                        Some(peer),
                        Instant::now(),
                        EditId::random(),
                        set_project(7, "later"),
                    )
                    .await
            }
        });
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert!(!later.is_finished(), "numbered before the earlier edit");
        assert_eq!(project_named(&coordinator.held().await.unwrap(), 7), None);
        drop(earlier);
        let stamp = tokio::time::timeout(Duration::from_secs(1), later)
            .await
            .expect("numbered once the earlier edit is done")
            .unwrap()
            .unwrap();
        let held = coordinator.held().await.unwrap();
        assert_eq!(
            held.stamps[&StampKey::Setting(SettingKey::Project(project(7, "").id))],
            stamp
        );
    }

    #[tokio::test]
    async fn a_refusal_held_for_an_id_whose_stamp_is_in_the_held_copy_is_not_reported() {
        let (_dir, coordinator) = alone();
        let (peer, id) = (MachineId::random(), EditId::random());
        let other = MachineId::random();
        let stamp = Stamp { seq: 5, by: other };
        let mut held = coordinator.held().await.unwrap();
        held.seq = 5;
        remember(&mut held, id, stamp);
        lock(&coordinator.store).apply_settings(held).unwrap();
        let _earlier = coordinator.wait_on(id);
        coordinator.sent_to(id, other);
        coordinator.take_answer(
            id,
            Outcome::Refused {
                reason: "no".into(),
                conflict: false,
            },
        );
        let made = coordinator
            .edit_from(Some(peer), Instant::now(), id, set_project(7, "p"))
            .await;
        assert_eq!(made.unwrap(), stamp);
    }

    #[tokio::test]
    async fn an_id_this_machine_numbered_keeps_its_stamp_over_later_numbers_and_a_late_refusal() {
        let (_dir, coordinator) = alone();
        let (peer, id) = (MachineId::random(), EditId::random());
        let stamp = coordinator
            .edit_from(Some(peer), Instant::now(), id, set_project(7, "p"))
            .await
            .unwrap();
        // It went to another machine before the role moved here, and that machine refuses it
        // late, once the shared copy has forgotten the id.
        coordinator.sent_to(id, MachineId::random());
        for n in 0..=NUMBERED_KEPT {
            let id = EditId::random();
            coordinator
                .edit_from(
                    Some(peer),
                    Instant::now(),
                    id,
                    set_project(8, &n.to_string()),
                )
                .await
                .unwrap();
        }
        let held = coordinator.held().await.unwrap();
        assert!(
            !held.numbered.contains_key(&id),
            "the shared copy forgot it"
        );
        coordinator.take_answer(
            id,
            Outcome::Refused {
                reason: "no".into(),
                conflict: false,
            },
        );
        let again = coordinator
            .edit_from(Some(peer), Instant::now(), id, set_project(7, "p"))
            .await;
        assert_eq!(again.unwrap(), stamp);
        assert_eq!(coordinator.held().await.unwrap().seq, held.seq);
    }

    // --- the project rules ---

    /// A coordinator alone holding `projects`.
    fn alone_with(projects: &[Project]) -> (tempfile::TempDir, Arc<Coordinator>) {
        let (dir, coordinator) = alone();
        lock(&coordinator.store)
            .apply_settings(SharedSettings {
                seq: 1,
                projects: projects.to_vec(),
                ..SharedSettings::default()
            })
            .unwrap();
        (dir, coordinator)
    }

    /// Stores a workspace another machine made from project `n`, live or archived.
    fn workspace_of(coordinator: &Coordinator, n: u128, archived: bool) {
        use foundry_proto::ids::WorkspaceId;
        use foundry_proto::record::{AnyRecord, Record, Workspace};
        let owner = MachineId::random();
        let record = AnyRecord::Workspace(Record {
            owner,
            version: 1,
            deleted: archived,
            body: Workspace {
                id: WorkspaceId::random(),
                project: project(n, "").id,
                name: "fix login".into(),
                branch: "fix-login".into(),
                base: "main".into(),
                setup: None,
            },
        });
        lock(&coordinator.store).apply(&record, owner).unwrap();
    }

    fn remove_project(n: u128) -> Edit {
        Edit::RemoveProject {
            id: project(n, "").id,
        }
    }

    /// The reason of a rule refusal, failing on any other end.
    fn conflict(made: Result<Stamp, CoordinatorError>) -> String {
        match made {
            Err(CoordinatorError::Conflict { reason }) => reason,
            other => panic!("not a rule refusal: {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_forwarded_add_of_a_name_another_project_has_is_refused_naming_no_value() {
        let held = project(1, "Foundry");
        let (_dir, coordinator) = alone_with(std::slice::from_ref(&held));
        let before = coordinator.held().await.unwrap();
        let mut add = project(2, "  fOUNDRY ");
        add.repo = "https://tom:hunter2@example.invalid/other.git".into();
        let made = coordinator
            .edit_from(
                Some(MachineId::random()),
                Instant::now(),
                EditId::random(),
                Edit::SetProject { project: add },
            )
            .await;
        let reason = conflict(made);
        assert!(reason.contains(&coordinator.me.to_string()), "{reason}");
        for value in ["oundry", "OUNDRY", "hunter2", "example.invalid"] {
            assert!(!reason.contains(value), "{value} in {reason}");
        }
        assert_eq!(coordinator.held().await.unwrap(), before);
    }

    #[tokio::test]
    async fn the_acting_machines_own_add_of_a_name_another_project_has_is_refused() {
        let (_dir, coordinator) = alone_with(&[project(1, "foundry")]);
        let made = coordinator
            .edit_from(
                None,
                Instant::now(),
                EditId::random(),
                set_project(2, "Foundry"),
            )
            .await;
        conflict(made);
        assert_eq!(coordinator.held().await.unwrap().projects.len(), 1);
    }

    #[tokio::test]
    async fn an_add_of_a_new_name_or_a_set_of_a_held_project_is_numbered() {
        let (_dir, coordinator) = alone_with(&[project(1, "foundry")]);
        for edit in [set_project(2, "api"), set_project(1, "foundry")] {
            coordinator
                .edit_from(
                    Some(MachineId::random()),
                    Instant::now(),
                    EditId::random(),
                    edit,
                )
                .await
                .unwrap();
        }
        assert_eq!(coordinator.held().await.unwrap().projects.len(), 2);
    }

    #[tokio::test]
    async fn a_forwarded_remove_of_a_project_with_a_live_workspace_is_refused_naming_no_value() {
        let held = project(1, "foundry");
        let (_dir, coordinator) = alone_with(std::slice::from_ref(&held));
        workspace_of(&coordinator, 1, false);
        let made = coordinator
            .edit_from(
                Some(MachineId::random()),
                Instant::now(),
                EditId::random(),
                remove_project(1),
            )
            .await;
        let reason = conflict(made);
        for value in ["foundry", "fix login", "example.invalid"] {
            assert!(!reason.contains(value), "{value} in {reason}");
        }
        assert_eq!(coordinator.held().await.unwrap().projects, [held]);
    }

    #[tokio::test]
    async fn a_remove_of_a_project_whose_workspaces_are_archived_or_elsewhere_is_numbered() {
        let (_dir, coordinator) = alone_with(&[project(1, "foundry")]);
        workspace_of(&coordinator, 1, true);
        workspace_of(&coordinator, 2, false);
        coordinator
            .edit_from(None, Instant::now(), EditId::random(), remove_project(1))
            .await
            .unwrap();
        assert!(coordinator.held().await.unwrap().projects.is_empty());
    }

    #[test]
    fn a_project_is_held_only_by_its_own_live_workspaces() {
        let (_dir, coordinator) = alone_with(&[project(1, "foundry"), project(2, "api")]);
        workspace_of(&coordinator, 1, true);
        workspace_of(&coordinator, 2, false);
        let store = lock(&coordinator.store);
        assert!(holding(&store, project(1, "").id).unwrap().is_empty());
        let held = holding(&store, project(2, "").id).unwrap();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].body.project, project(2, "").id);
    }

    #[tokio::test(start_paused = true)]
    async fn a_rule_refusal_is_not_reported_while_another_task_waits_on_its_id() {
        let (_dir, coordinator) = alone_with(&[project(1, "foundry")]);
        let id = EditId::random();
        // Another task here may number the id, so a refusal here says nothing final about it.
        let _other = coordinator.wait_on(id);
        let made = coordinator
            .edit_from(
                Some(MachineId::random()),
                Instant::now(),
                id,
                set_project(2, "foundry"),
            )
            .await;
        assert!(
            !matches!(made, Ok(_) | Err(CoordinatorError::Conflict { .. })),
            "{made:?}"
        );
        assert_eq!(coordinator.held().await.unwrap().projects.len(), 1);
    }

    #[tokio::test]
    async fn the_acting_machine_refuses_to_list_a_machine_its_copy_shows_removed() {
        let (_dir, coordinator) = alone();
        let c = MachineId::random();
        let entry = |address: &str| Edit::SetMachine {
            machine: Machine {
                id: c,
                name: "c".into(),
                address: address.into(),
                key: String::new(),
            },
        };
        for edit in [entry("100.64.0.9"), Edit::RemoveMachine { id: c }] {
            coordinator
                .number(Some(EditId::random()), &edit)
                .await
                .unwrap_or_else(|_| panic!("not numbered"));
        }

        let again = coordinator
            .number(Some(EditId::random()), &entry("100.64.1.1"))
            .await;

        assert!(
            matches!(again, Err(Unnumbered::Breaks(_))),
            "a removed machine was listed again"
        );
        assert!(crate::join::is_removed(
            &coordinator.held().await.unwrap(),
            c
        ));
    }

    #[tokio::test]
    async fn an_edit_with_no_id_from_an_older_machine_is_numbered_unchecked() {
        let (_dir, coordinator) = alone_with(&[project(1, "foundry")]);
        // Nobody answers it, so a refusal would drop it without a word.
        coordinator
            .number(None, &set_project(2, "foundry"))
            .await
            .unwrap_or_else(|_| panic!("not numbered"));
        assert_eq!(coordinator.held().await.unwrap().projects.len(), 2);
    }

    #[tokio::test]
    async fn a_rule_refusal_answered_to_this_machine_ends_its_edit_as_a_rule_refusal() {
        let (_dir, coordinator) = alone();
        let (peer, id) = (MachineId::random(), EditId::random());
        let _waiting = coordinator.wait_on(id);
        coordinator.sent_to(id, peer);
        let refused = Outcome::Refused {
            reason: format!("{peer} holds another project with that name"),
            conflict: true,
        };
        coordinator.take_answer(id, refused);
        // It acts here too, but the answer is read first.
        let made = coordinator
            .run_edit(None, Instant::now(), id, set_project(7, "p"))
            .await;
        assert_eq!(
            conflict(made),
            format!("{peer} holds another project with that name")
        );
    }
}
