//! Account-scoped playback measurement and bounded, ordered NCBL delivery.
use log::{debug, warn};
use ncm_api::{MusicApi, PlaybackEvent, PlaybackEventKind, ScrobbleContext, ScrobbleError};
use std::{
    cell::RefCell,
    collections::VecDeque,
    rc::Rc,
    time::{Duration, Instant, SystemTime},
};

const CAPACITY: usize = 128;
const MAX_SAMPLE_GAP: Duration = Duration::from_secs(5);

/// FIFO URI-loaded boundary associates adapter callbacks with an accepted load.
#[derive(Debug, Default)]
pub struct PlaybackGate {
    requested: u64,
    uri: String,
    active: Option<u64>,
    stopped: bool,
}
impl PlaybackGate {
    pub fn request(&mut self, generation: u64, uri: &str) {
        self.requested = generation;
        self.uri = uri.to_owned();
        self.active = None;
        self.stopped = false;
    }
    pub fn loaded(&mut self, uri: &str) {
        if uri == self.uri {
            self.active = Some(self.requested);
            self.stopped = false;
        }
    }
    pub fn active(&self) -> Option<u64> {
        self.active
    }
    pub fn mark_stopped(&mut self) {
        if self.active.is_some() {
            self.stopped = true;
        }
    }
    pub fn needs_reload(&self) -> bool {
        self.stopped
    }
    pub fn stop(&mut self) {
        self.active = None;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    Change,
    Eos,
    Stop,
    Error,
    Logout,
    Quit,
}

#[derive(Debug, Clone)]
pub enum PlaybackObservation {
    Playing(bool),
    Buffering(bool),
    Position(Duration),
    Duration(Duration),
    Seek,
    SeekDone(Duration),
}

#[derive(Default, Clone)]
struct PlaybackClock {
    playing: bool,
    buffering: bool,
    seeking: bool,
    position: Duration,
    checkpoint: Option<(Instant, Duration)>,
    played: Duration,
}
impl PlaybackClock {
    fn observe(&mut self, event: &PlaybackObservation, now: Instant) -> Duration {
        let before = self.played;
        match *event {
            PlaybackObservation::Playing(playing) => {
                self.playing = playing;
                self.checkpoint = Some((now, self.position));
            }
            PlaybackObservation::Buffering(buffering) => {
                self.buffering = buffering;
                self.checkpoint = None;
            }
            PlaybackObservation::Seek => {
                self.seeking = true;
                self.checkpoint = None;
            }
            PlaybackObservation::SeekDone(position) => {
                self.position = position;
                self.seeking = false;
                self.checkpoint = Some((now, position));
            }
            PlaybackObservation::Position(position) => {
                if self.playing && !self.buffering && !self.seeking {
                    if let Some((last, previous)) = self.checkpoint {
                        let wall = now.saturating_duration_since(last);
                        if let Some(media) = position.checked_sub(previous) {
                            if wall <= MAX_SAMPLE_GAP && media <= wall + Duration::from_millis(250)
                            {
                                self.played += wall.min(media);
                            }
                        }
                    }
                }
                self.position = position;
                self.checkpoint = Some((now, position));
            }
            PlaybackObservation::Duration(_) => (),
        }
        self.played.saturating_sub(before)
    }
}

#[derive(Clone)]
struct Account {
    api: MusicApi,
    context: ScrobbleContext,
    generation: u64,
}

#[derive(Clone)]
struct Session {
    generation: u64,
    event: PlaybackEvent,
    clock: PlaybackClock,
    confirmed: bool,
    deliver: bool,
    ended: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeliveryState {
    Pending,
    InFlight,
    Confirmed,
    Unknown,
    Terminal,
}
impl DeliveryState {
    fn settled(self) -> bool {
        matches!(self, Self::Confirmed | Self::Unknown | Self::Terminal)
    }
}
struct Delivery {
    account: Account,
    start: PlaybackEvent,
    end: Option<PlaybackEvent>,
    start_state: DeliveryState,
    end_state: DeliveryState,
}
#[derive(Clone)]
struct Job {
    account: Account,
    event: PlaybackEvent,
}

struct Inner {
    enabled: bool,
    account_generation: u64,
    next_session: u64,
    account: Option<Account>,
    session: Option<Session>,
    deliveries: VecDeque<Delivery>,
    in_flight: bool,
    shutting_down: bool,
}
impl Default for Inner {
    fn default() -> Self {
        Self {
            enabled: true,
            account_generation: 0,
            next_session: 0,
            account: None,
            session: None,
            deliveries: VecDeque::new(),
            shutting_down: false,
            in_flight: false,
        }
    }
}
impl Inner {
    fn enqueue_start(&mut self, event: PlaybackEvent) -> bool {
        let Some(account) = self.account.clone() else {
            return false;
        };
        if self.deliveries.len() * 2 >= CAPACITY {
            if let Some(index) = self
                .deliveries
                .iter()
                .position(|d| d.start_state == DeliveryState::Pending)
            {
                if let Some(old) = self.deliveries.remove(index) {
                    warn!(
                        "scrobble session={} terminal=queue-capacity",
                        old.start.session_id
                    );
                }
            } else {
                warn!(
                    "scrobble session={} terminal=queue-capacity",
                    event.session_id
                );
                return false;
            }
        }
        self.deliveries.push_back(Delivery {
            account,
            start: event,
            end: None,
            start_state: DeliveryState::Pending,
            end_state: DeliveryState::Pending,
        });
        true
    }
    fn next_job(&mut self) -> Option<Job> {
        if self.in_flight {
            return None;
        }
        for delivery in &mut self.deliveries {
            if delivery.start_state == DeliveryState::Pending {
                delivery.start_state = DeliveryState::InFlight;
                self.in_flight = true;
                return Some(Job {
                    account: delivery.account.clone(),
                    event: delivery.start.clone(),
                });
            }
            if delivery.start_state.settled() && delivery.end_state == DeliveryState::Pending {
                if let Some(event) = &delivery.end {
                    delivery.end_state = DeliveryState::InFlight;
                    self.in_flight = true;
                    return Some(Job {
                        account: delivery.account.clone(),
                        event: event.clone(),
                    });
                }
            }
        }
        None
    }
    fn acknowledge(&mut self, job: &Job, state: DeliveryState) {
        self.in_flight = false;
        if let Some(delivery) = self.deliveries.iter_mut().find(|d| {
            d.account.generation == job.account.generation
                && d.start.session_id == job.event.session_id
        }) {
            match job.event.kind {
                PlaybackEventKind::Start => delivery.start_state = state,
                PlaybackEventKind::End => delivery.end_state = state,
            }
        }
        self.deliveries.retain(|d| !d.end_state.settled());
    }
    fn reset_measurement(&mut self) {
        if let Some(session) = &mut self.session {
            self.next_session += 1;
            session.event.session_id = self.next_session;
            session.clock.played = Duration::ZERO;
            session.clock.checkpoint = None;
            session.confirmed = false;
            session.deliver = true;
        }
    }
    fn finish(&mut self, reason: EndReason) {
        if let Some(session) = &mut self.session {
            if session.ended {
                return;
            }
            session.ended = true;
            session.clock.playing = false;
            if session.confirmed && session.deliver {
                let mut event = session.event.clone();
                event.kind = PlaybackEventKind::End;
                event.played = session.clock.played;
                event.timestamp = SystemTime::now();
                if let Some(delivery) = self
                    .deliveries
                    .iter_mut()
                    .find(|d| d.start.session_id == event.session_id)
                {
                    debug!(
                        "scrobble session={} end={reason:?} seconds={}",
                        event.session_id,
                        event.played.as_secs()
                    );
                    delivery.end = Some(event);
                }
            }
        }
    }
}

/// Main-context handle. The worker owns frozen credentials only while sending.
#[derive(Clone)]
pub struct Scrobbler {
    inner: Rc<RefCell<Inner>>,
    wake: async_channel::Sender<()>,
    receiver: async_channel::Receiver<()>,
}
impl Default for Scrobbler {
    fn default() -> Self {
        let (wake, receiver) = async_channel::bounded(1);
        Self {
            inner: Rc::new(RefCell::new(Inner::default())),
            wake,
            receiver,
        }
    }
}
impl Scrobbler {
    fn notify(&self) {
        let _ = self.wake.try_send(());
    }
    pub fn set_account(&self, api: MusicApi, uid: u64) {
        let mut inner = self.inner.borrow_mut();
        let media = inner.session.clone();
        inner.finish(EndReason::Logout);
        inner.deliveries.clear();
        inner.session = media;
        inner.reset_measurement();
        inner.account_generation += 1;
        inner.account = api.scrobble_context(uid).ok().map(|context| Account {
            api,
            context,
            generation: inner.account_generation,
        });
        if inner.account.is_some() {
            debug!(
                "scrobble account generation={} ready",
                inner.account_generation
            );
        }
    }
    pub fn logout(&self) {
        let mut inner = self.inner.borrow_mut();
        let media = inner.session.clone();
        inner.finish(EndReason::Logout);
        inner.deliveries.clear();
        inner.session = media;
        inner.reset_measurement();
        inner.account = None;
        inner.account_generation += 1;
    }
    pub fn set_enabled(&self, enabled: bool) {
        let mut inner = self.inner.borrow_mut();
        if inner.enabled == enabled {
            return;
        }
        inner.enabled = enabled;
        inner.deliveries.clear();
        if let Some(session) = &mut inner.session {
            session.clock.played = Duration::ZERO;
            session.clock.checkpoint = None;
            session.confirmed = false;
            session.deliver = true;
        }
        if enabled {
            inner.next_session += 1;
            let id = inner.next_session;
            if let Some(session) = &mut inner.session {
                session.event.session_id = id;
            }
        }
    }
    pub fn begin(&self, generation: u64, mut event: PlaybackEvent) {
        let mut inner = self.inner.borrow_mut();
        inner.finish(EndReason::Change);
        inner.next_session += 1;
        event.session_id = inner.next_session;
        inner.session = Some(Session {
            generation,
            event,
            clock: PlaybackClock::default(),
            confirmed: false,
            deliver: true,
            ended: false,
        });
        drop(inner);
        self.notify();
    }
    pub fn observe(&self, generation: u64, observation: PlaybackObservation, now: Instant) {
        let mut inner = self.inner.borrow_mut();
        let enabled = inner.enabled && inner.account.is_some() && !inner.shutting_down;
        if inner
            .session
            .as_ref()
            .is_some_and(|s| s.generation == generation && s.ended)
            && matches!(observation, PlaybackObservation::Playing(true))
        {
            inner.next_session += 1;
            let id = inner.next_session;
            if let Some(session) = &mut inner.session {
                session.event.session_id = id;
                session.clock = PlaybackClock::default();
                session.confirmed = false;
                session.deliver = true;
                session.ended = false;
            }
        }
        let Some(session) = &mut inner.session else {
            return;
        };
        if session.generation != generation || session.ended {
            return;
        }
        if let PlaybackObservation::Duration(duration) = observation {
            session.event.duration = duration;
        }
        let delta = session.clock.observe(&observation, now);
        if !enabled {
            session.clock.played = Duration::ZERO;
            return;
        }
        if delta > Duration::ZERO && !session.confirmed && session.event.song_id > 0 {
            session.confirmed = true;
            session.event.timestamp = SystemTime::now();
            let event = session.event.clone();
            let deliver = inner.enqueue_start(event);
            if let Some(session) = &mut inner.session {
                session.deliver = deliver;
            }
        }
        drop(inner);
        self.notify();
    }
    pub fn finish(&self, generation: u64, reason: EndReason) {
        let mut inner = self.inner.borrow_mut();
        if inner
            .session
            .as_ref()
            .is_some_and(|s| s.generation == generation)
        {
            inner.finish(reason);
        }
        drop(inner);
        self.notify();
    }
    pub fn shutdown(&self) {
        let mut inner = self.inner.borrow_mut();
        inner.shutting_down = true;
        inner.finish(EndReason::Quit);
        drop(inner);
        self.notify();
    }
    pub fn is_idle(&self) -> bool {
        let inner = self.inner.borrow();
        inner.deliveries.is_empty() && !inner.in_flight
    }
    pub async fn run(self) {
        while self.receiver.recv().await.is_ok() {
            loop {
                let job = self.inner.borrow_mut().next_job();
                let Some(job) = job else {
                    break;
                };
                let result = job
                    .account
                    .api
                    .scrobble(&job.account.context, &job.event)
                    .await;
                let state = match result {
                    Ok(_) => DeliveryState::Confirmed,
                    Err(ScrobbleError::Transport(_) | ScrobbleError::Unknown) => {
                        DeliveryState::Unknown
                    }
                    Err(_) => DeliveryState::Terminal,
                };
                debug!(
                    "scrobble generation={} session={} event={:?} state={state:?}",
                    job.account.generation, job.event.session_id, job.event.kind
                );
                if let Err(error) = result {
                    warn!("scrobble session={} {error}", job.event.session_id);
                }
                self.inner.borrow_mut().acknowledge(&job, state);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ncm_api::{CookieBuilder, CookieJar, PlaybackSource};
    #[test]
    fn uri_boundaries_route_accepted_media_generations() {
        let mut gate = PlaybackGate::default();
        gate.request(1, "file:///one.wav");
        gate.loaded("file:///one.wav");
        assert_eq!(gate.active(), Some(1));
        gate.request(2, "file:///two.wav");
        gate.loaded("file:///one.wav");
        gate.loaded("file:///two.wav");
        assert_eq!(gate.active(), Some(2));
    }

    #[test]
    fn restarting_stopped_media_creates_a_fresh_pair() {
        let s = service();
        let now = Instant::now();
        s.begin(1, event());
        for offset in [0, 10] {
            s.observe(
                1,
                PlaybackObservation::Playing(true),
                now + Duration::from_secs(offset),
            );
            s.observe(
                1,
                PlaybackObservation::Position(Duration::from_secs(2)),
                now + Duration::from_secs(offset + 2),
            );
            s.finish(1, EndReason::Stop);
        }
        let inner = s.inner.borrow();
        assert_eq!(
            inner
                .deliveries
                .iter()
                .map(|d| (d.start.session_id, d.end.as_ref().unwrap().played))
                .collect::<Vec<_>>(),
            vec![(1, Duration::from_secs(2)), (2, Duration::from_secs(2))]
        );
    }

    #[test]
    fn account_switch_preserves_in_flight_owner_and_rebases_live_playback() {
        let s = service();
        let now = Instant::now();
        s.begin(1, event());
        s.observe(1, PlaybackObservation::Playing(true), now);
        s.observe(
            1,
            PlaybackObservation::Position(Duration::from_secs(2)),
            now + Duration::from_secs(2),
        );
        let old_job = s.inner.borrow_mut().next_job().unwrap();
        s.set_account(old_job.account.api.clone(), 8);
        s.observe(
            1,
            PlaybackObservation::Position(Duration::from_secs(3)),
            now + Duration::from_secs(3),
        );
        s.observe(
            1,
            PlaybackObservation::Position(Duration::from_secs(5)),
            now + Duration::from_secs(5),
        );
        s.finish(1, EndReason::Change);
        s.inner
            .borrow_mut()
            .acknowledge(&old_job, DeliveryState::Confirmed);
        let next = s.inner.borrow_mut().next_job().unwrap();
        assert_eq!(old_job.account.context.account_id, 7);
        assert_eq!(next.account.context.account_id, 8);
        assert_eq!(
            s.inner
                .borrow()
                .deliveries
                .front()
                .unwrap()
                .end
                .as_ref()
                .unwrap()
                .played,
            Duration::from_secs(2)
        );
    }
    fn event() -> PlaybackEvent {
        PlaybackEvent {
            session_id: 0,
            kind: PlaybackEventKind::Start,
            song_id: 42,
            duration: Duration::from_secs(180),
            played: Duration::ZERO,
            source: PlaybackSource::default(),
            bitrate: 0,
            quality: String::new(),
            timestamp: SystemTime::now(),
        }
    }
    fn service() -> Scrobbler {
        let jar = CookieJar::new();
        let uri = "https://music.163.com/".parse().unwrap();
        jar.set(
            CookieBuilder::new("MUSIC_U", "fixture")
                .domain("music.163.com")
                .path("/")
                .build()
                .unwrap(),
            &uri,
        )
        .unwrap();
        let s = Scrobbler::default();
        s.set_account(MusicApi::from_cookie_jar(jar, 1), 7);
        s
    }
    #[test]
    fn measures_playback_across_pause_seek_and_resume() {
        let now = Instant::now();
        let mut clock = PlaybackClock::default();
        let mut apply = |seconds, event| {
            clock.observe(&event, now + Duration::from_secs(seconds));
        };
        apply(0, PlaybackObservation::Playing(true));
        apply(2, PlaybackObservation::Position(Duration::from_secs(2)));
        apply(2, PlaybackObservation::Playing(false));
        apply(12, PlaybackObservation::Playing(true));
        apply(14, PlaybackObservation::Position(Duration::from_secs(4)));
        apply(14, PlaybackObservation::Seek);
        apply(15, PlaybackObservation::SeekDone(Duration::from_secs(60)));
        apply(17, PlaybackObservation::Position(Duration::from_secs(62)));
        apply(17, PlaybackObservation::Buffering(true));
        apply(27, PlaybackObservation::Position(Duration::from_secs(62)));
        apply(27, PlaybackObservation::Buffering(false));
        apply(28, PlaybackObservation::Position(Duration::from_secs(63)));
        apply(30, PlaybackObservation::Position(Duration::from_secs(65)));
        apply(100, PlaybackObservation::Position(Duration::from_secs(135)));
        apply(102, PlaybackObservation::Position(Duration::from_secs(137)));
        apply(102, PlaybackObservation::Seek);
        apply(103, PlaybackObservation::SeekDone(Duration::from_secs(20)));
        apply(105, PlaybackObservation::Position(Duration::from_secs(22)));
        assert_eq!(clock.played, Duration::from_secs(12));
    }
    #[test]
    fn delivery_pairs_each_completed_playback_and_loop() {
        let s = service();
        let now = Instant::now();
        for generation in [1, 2] {
            s.begin(generation, event());
            s.observe(generation, PlaybackObservation::Playing(true), now);
            s.observe(
                generation,
                PlaybackObservation::Position(Duration::from_secs(2)),
                now + Duration::from_secs(2),
            );
            s.finish(generation, EndReason::Eos);
        }
        let mut inner = s.inner.borrow_mut();
        let mut delivered = Vec::new();
        while let Some(job) = inner.next_job() {
            delivered.push((job.event.session_id, job.event.kind, job.event.played));
            inner.acknowledge(&job, DeliveryState::Confirmed);
        }
        assert_eq!(
            delivered,
            vec![
                (1, PlaybackEventKind::Start, Duration::ZERO),
                (1, PlaybackEventKind::End, Duration::from_secs(2)),
                (2, PlaybackEventKind::Start, Duration::ZERO),
                (2, PlaybackEventKind::End, Duration::from_secs(2))
            ]
        );
    }
    #[test]
    fn preference_and_account_generations_start_fresh_measurements() {
        let s = service();
        let now = Instant::now();
        s.begin(1, event());
        s.observe(1, PlaybackObservation::Playing(true), now);
        s.observe(
            1,
            PlaybackObservation::Position(Duration::from_secs(2)),
            now + Duration::from_secs(2),
        );
        s.set_enabled(false);
        s.set_enabled(true);
        s.observe(
            1,
            PlaybackObservation::Position(Duration::from_secs(3)),
            now + Duration::from_secs(3),
        );
        s.observe(
            1,
            PlaybackObservation::Position(Duration::from_secs(5)),
            now + Duration::from_secs(5),
        );
        s.finish(1, EndReason::Stop);
        let inner = s.inner.borrow();
        let d = inner.deliveries.front().unwrap();
        assert_eq!(d.start.session_id, 2);
        assert_eq!(d.end.as_ref().unwrap().played, Duration::from_secs(2));
        assert_eq!(d.account.context.account_id, 7);
    }
    #[test]
    fn queue_reserves_complete_pairs_within_capacity() {
        let s = service();
        let mut inner = s.inner.borrow_mut();
        for id in 1..=100 {
            let mut e = event();
            e.session_id = id;
            inner.enqueue_start(e);
        }
        assert_eq!(inner.deliveries.len() * 2, CAPACITY);
        assert_eq!(inner.deliveries.front().unwrap().start.session_id, 37);
        assert_eq!(inner.deliveries.back().unwrap().start.session_id, 100);
    }
}
