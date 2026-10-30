//! Android-style audio focus arbitration.
//!
//! All decisions are made on one thread: D-Bus events and socket commands are
//! applied in order by the daemon's main loop, so no locking is needed.

use crate::config::Config;
use crate::mpris::{matches_any, Event, Player, Source};
use crate::protocol::{FocusInfo, PlayerStatus, Request, Response, Status};
use std::collections::{BTreeSet, HashMap};
use std::time::{Duration, Instant};

/// Focus grant mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Automatic focus: the newest playing player wins.
    Exclusive,
    /// A trigger (call, alarm, ...) holds everything paused until it ends.
    Transient,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Exclusive => "exclusive",
            Mode::Transient => "transient",
        }
    }
}

struct Focus {
    owner: String,
    mode: Mode,
    locked: bool,
    auto: bool,
    reason: String,
    since: Instant,
    resume_on_release: bool,
}

/// Focus policy state. Methods take a [`Source`] so the same logic runs
/// against D-Bus in the daemon and a fake in tests.
pub struct FocusManager {
    cfg: Config,
    focus: Option<Focus>,
    paused: BTreeSet<String>,
    grace: HashMap<String, Instant>,
    deadline: Option<Instant>,
}

impl FocusManager {
    pub fn new(cfg: Config) -> Self {
        Self {
            cfg,
            focus: None,
            paused: BTreeSet::new(),
            grace: HashMap::new(),
            deadline: None,
        }
    }

    /// Pick up players that are already playing when the daemon starts.
    pub fn adopt(&mut self, src: &dyn Source, now: Instant) {
        let players = src.players();

        // A trigger that is already playing (call in progress, alarm ringing)
        // holds the lock immediately and pauses everything else.
        if let Some(player) = players
            .iter()
            .find(|player| player.status == "Playing" && self.is_trigger(player))
        {
            eprintln!(
                "lafd: adopting active trigger {} ({})",
                player.identity, player.name
            );
            let reason = format!("trigger: {}", player.identity);
            if let Err(err) = self.acquire(
                player.name.clone(),
                Mode::Transient,
                true,
                true,
                reason,
                true,
                Some(player.name.as_str()),
                self.cfg.trigger_timeout_ms,
                src,
                now,
            ) {
                eprintln!("lafd: could not adopt active trigger: {err}");
            }
            return;
        }

        let playing: Vec<&Player> = players
            .iter()
            .filter(|player| player.status == "Playing")
            .collect();
        match playing.len() {
            0 => {}
            1 => {
                let player = playing[0];
                eprintln!(
                    "lafd: adopted existing playback: {} ({})",
                    player.identity, player.name
                );
                self.set_auto_focus(&player.name, "adopted at startup", now);
            }
            count => eprintln!(
                "lafd: {count} players are already playing; waiting for the next transition"
            ),
        }
    }

    /// Apply one player set change.
    pub fn on_event(&mut self, event: Event, src: &dyn Source, now: Instant) {
        match event {
            Event::Added(player) | Event::Changed(player) => self.on_player(&player, src, now),
            Event::Removed(name) => self.on_removed(&name, src, now),
        }
    }

    /// Release an expired trigger lock. Call once per main-loop iteration.
    pub fn tick(&mut self, src: &dyn Source, now: Instant) {
        let Some(deadline) = self.deadline else {
            return;
        };
        if now < deadline {
            return;
        }
        self.deadline = None;
        let resume = self
            .focus
            .as_ref()
            .map(|focus| focus.resume_on_release)
            .unwrap_or(false);
        self.release(resume, "timeout", src, now);
    }

    /// Time until the next trigger timeout, if any.
    pub fn time_until_deadline(&self, now: Instant) -> Option<Duration> {
        self.deadline
            .map(|deadline| deadline.saturating_duration_since(now))
    }

    pub fn status(&self, src: &dyn Source, now: Instant) -> Status {
        let focus = self.focus.as_ref().map(|focus| FocusInfo {
            owner: focus.owner.clone(),
            mode: focus.mode.as_str().to_string(),
            locked: focus.locked,
            auto: focus.auto,
            reason: focus.reason.clone(),
            since_secs: now.saturating_duration_since(focus.since).as_secs(),
        });

        let players = src
            .players()
            .into_iter()
            .map(|player| PlayerStatus {
                is_focus: self
                    .focus
                    .as_ref()
                    .is_some_and(|focus| focus.owner == player.name),
                paused_by_service: self.paused.contains(&player.name),
                name: player.name,
                identity: player.identity,
                status: player.status,
            })
            .collect();

        Status {
            focus,
            paused_by_service: self.paused.iter().cloned().collect(),
            players,
        }
    }

    /// Execute one socket command.
    pub fn handle(&mut self, req: &Request, src: &dyn Source, now: Instant) -> Response {
        match req.cmd.as_str() {
            "ping" => Response::ok(serde_json::json!({ "pong": true })),
            "status" => Response::ok(self.status(src, now)),
            "trigger_start" => self.cmd_trigger_start(req, src, now),
            "trigger_end" => self.cmd_trigger_end(req, src, now),
            other => Response::err(format!(
                "unknown command {other:?} (want status, trigger_start, trigger_end or ping)"
            )),
        }
    }

    fn cmd_trigger_start(&mut self, req: &Request, src: &dyn Source, now: Instant) -> Response {
        if let Some(focus) = self.focus.as_ref().filter(|focus| focus.locked) {
            return Response::err(format!("focus is already locked by {:?}", focus.owner));
        }
        let name = req
            .name
            .clone()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| "trigger".to_string());
        let timeout = req
            .timeout_ms
            .filter(|timeout| *timeout > 0)
            .unwrap_or(self.cfg.trigger_timeout_ms);

        match self.acquire(
            name,
            Mode::Transient,
            true,
            false,
            "manual trigger".to_string(),
            true,
            None,
            timeout,
            src,
            now,
        ) {
            Ok(()) => Response::ok(self.status(src, now)),
            Err(err) => Response::err(err),
        }
    }

    fn cmd_trigger_end(&mut self, req: &Request, src: &dyn Source, now: Instant) -> Response {
        let Some(focus) = self.focus.as_ref() else {
            return Response::err("no trigger is active");
        };
        if focus.mode != Mode::Transient {
            return Response::err("no trigger is active");
        }
        if let Some(name) = req.name.as_deref().filter(|name| !name.is_empty()) {
            if focus.owner != name {
                return Response::err(format!("active trigger is {:?}, not {name:?}", focus.owner));
            }
        }
        let resume = focus.resume_on_release;
        self.release(resume, "trigger ended", src, now);
        Response::ok(self.status(src, now))
    }

    fn on_player(&mut self, player: &Player, src: &dyn Source, now: Instant) {
        match player.status.as_str() {
            "Playing" => self.on_playing(player, src, now),
            "Paused" | "Stopped" => self.on_not_playing(player, src, now),
            _ => {}
        }
    }

    fn on_playing(&mut self, player: &Player, src: &dyn Source, now: Instant) {
        if let Some(expiry) = self.grace.get(&player.name).copied() {
            if now < expiry {
                return;
            }
            self.grace.remove(&player.name);
        }

        // A player that starts again is no longer waiting to be resumed.
        self.paused.remove(&player.name);

        // Trigger players interrupt automatic focus but never a manual lock.
        if self.is_trigger(player) && !self.focus.as_ref().is_some_and(|focus| focus.locked) {
            let reason = format!("trigger: {}", player.identity);
            if let Err(err) = self.acquire(
                player.name.clone(),
                Mode::Transient,
                true,
                true,
                reason,
                true,
                Some(player.name.as_str()),
                self.cfg.trigger_timeout_ms,
                src,
                now,
            ) {
                eprintln!("lafd: could not acquire trigger focus: {err}");
            }
            return;
        }

        enum Action {
            SetFocus,
            Keep,
            Pause,
            Steal,
        }

        let action = match self.focus.as_ref() {
            None => Action::SetFocus,
            Some(focus) if focus.owner == player.name => Action::Keep,
            Some(focus) if focus.locked => Action::Pause,
            Some(_) => Action::Steal,
        };

        match action {
            Action::Keep => {}
            Action::SetFocus => {
                eprintln!("lafd: focus set to {} ({})", player.identity, player.name);
                self.set_auto_focus(&player.name, "playback", now);
                self.pause_others(Some(&player.name), src);
            }
            Action::Pause => {
                // A lock is active: new players are paused instead of stealing
                // focus.
                let name = player.name.clone();
                self.pause_player(&name, src);
            }
            Action::Steal => {
                eprintln!("lafd: focus moved to {} ({})", player.identity, player.name);
                self.set_auto_focus(&player.name, "playback", now);
                self.pause_others(Some(&player.name), src);
            }
        }
    }

    fn on_not_playing(&mut self, player: &Player, src: &dyn Source, now: Instant) {
        if player.status == "Stopped" {
            self.paused.remove(&player.name);
            self.grace.remove(&player.name);
        }

        let Some(focus) = self.focus.as_ref() else {
            return;
        };
        if focus.owner != player.name {
            return;
        }

        if focus.locked {
            // Automatic trigger locks end with the trigger player. Manual
            // locks stay until trigger_end or the timeout.
            if focus.auto {
                let resume = focus.resume_on_release;
                self.release(resume, "trigger finished", src, now);
            }
            return;
        }
        self.release(false, "focus owner paused", src, now);
    }

    fn on_removed(&mut self, name: &str, src: &dyn Source, now: Instant) {
        self.paused.remove(name);
        self.grace.remove(name);

        let auto_owner = self
            .focus
            .as_ref()
            .is_some_and(|focus| focus.owner == name && focus.auto);
        if auto_owner {
            let resume = self
                .focus
                .as_ref()
                .map(|focus| focus.resume_on_release)
                .unwrap_or(false);
            self.release(resume, "focus owner disappeared", src, now);
        }
    }

    fn set_auto_focus(&mut self, owner: &str, reason: &str, now: Instant) {
        self.focus = Some(Focus {
            owner: owner.to_string(),
            mode: Mode::Exclusive,
            locked: false,
            auto: true,
            reason: reason.to_string(),
            since: now,
            resume_on_release: false,
        });
        self.deadline = None;
    }

    #[allow(clippy::too_many_arguments)]
    fn acquire(
        &mut self,
        owner: String,
        mode: Mode,
        locked: bool,
        auto: bool,
        reason: String,
        resume: bool,
        except: Option<&str>,
        timeout_ms: u64,
        src: &dyn Source,
        now: Instant,
    ) -> Result<(), String> {
        if let Some(focus) = self.focus.as_ref().filter(|focus| focus.locked) {
            return Err(format!("focus is already locked by {:?}", focus.owner));
        }

        // The previous automatic owner loses; players it paused stay paused
        // but are no longer tracked for resume.
        self.paused.clear();
        self.deadline = None;
        eprintln!(
            "lafd: focus acquired by {owner} (mode={}, locked={locked}, auto={auto}, reason={reason})",
            mode.as_str()
        );
        self.focus = Some(Focus {
            owner,
            mode,
            locked,
            auto,
            reason,
            since: now,
            resume_on_release: resume,
        });

        self.pause_others(except, src);

        if timeout_ms > 0 {
            self.deadline = Some(now + Duration::from_millis(timeout_ms));
        }
        Ok(())
    }

    fn release(&mut self, resume: bool, reason: &str, src: &dyn Source, now: Instant) {
        let Some(focus) = self.focus.take() else {
            return;
        };
        self.deadline = None;
        eprintln!(
            "lafd: focus released by {} (reason={reason}, resume={resume})",
            focus.owner
        );

        if resume {
            self.resume_paused(src, now);
        } else {
            self.paused.clear();
        }
    }

    fn pause_others(&mut self, except: Option<&str>, src: &dyn Source) {
        let names: Vec<String> = src
            .players()
            .into_iter()
            .filter(|player| player.status == "Playing")
            .filter(|player| Some(player.name.as_str()) != except)
            .map(|player| player.name)
            .collect();
        for name in names {
            self.pause_player(&name, src);
        }
    }

    fn pause_player(&mut self, name: &str, src: &dyn Source) {
        if self.paused.contains(name) {
            return;
        }
        let Some(player) = src.get(name) else {
            return;
        };
        if player.status != "Playing" {
            return;
        }

        self.paused.insert(name.to_string());
        eprintln!("lafd: pausing {} ({})", player.identity, name);
        if let Err(err) = src.pause(name) {
            self.paused.remove(name);
            eprintln!("lafd: could not pause {name}: {err}");
        }
    }

    fn resume_paused(&mut self, src: &dyn Source, now: Instant) {
        if self.paused.is_empty() {
            return;
        }

        let names: Vec<String> = self.paused.iter().cloned().collect();
        self.paused.clear();

        let grace = Duration::from_millis(self.cfg.resume_grace_ms);
        let mut first: Option<String> = None;
        for name in names {
            let Some(player) = src.get(&name) else {
                continue;
            };
            if player.status != "Paused" {
                continue;
            }
            if first.is_none() {
                first = Some(name.clone());
            }
            if !grace.is_zero() {
                self.grace.insert(name.clone(), now + grace);
            }
            eprintln!("lafd: resuming {} ({})", player.identity, name);
            if let Err(err) = src.play(&name) {
                eprintln!("lafd: could not resume {name}: {err}");
            }
        }

        if let Some(owner) = first {
            self.set_auto_focus(&owner, "resumed after focus loss", now);
        }
    }

    fn is_trigger(&self, player: &Player) -> bool {
        matches_any(
            &self.cfg.trigger_players,
            &[player.name.as_str(), player.identity.as_str()],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    struct FakeSource {
        players: RefCell<BTreeMap<String, Player>>,
        paused: RefCell<Vec<String>>,
        played: RefCell<Vec<String>>,
    }

    impl FakeSource {
        fn new(initial: &[(&str, &str, &str)]) -> Self {
            let mut players = BTreeMap::new();
            for (suffix, identity, status) in initial {
                let name = format!("org.mpris.MediaPlayer2.{suffix}");
                players.insert(
                    name.clone(),
                    Player {
                        name,
                        identity: (*identity).to_string(),
                        status: (*status).to_string(),
                        owner: format!(":1.{suffix}"),
                    },
                );
            }
            Self {
                players: RefCell::new(players),
                paused: RefCell::new(Vec::new()),
                played: RefCell::new(Vec::new()),
            }
        }

        fn set_status(&self, suffix: &str, status: &str) {
            let name = format!("org.mpris.MediaPlayer2.{suffix}");
            let mut players = self.players.borrow_mut();
            players.get_mut(&name).expect("player exists").status = status.to_string();
        }

        fn event(&self, suffix: &str) -> Event {
            let name = format!("org.mpris.MediaPlayer2.{suffix}");
            let player = self.players.borrow().get(&name).cloned().expect("player");
            Event::Changed(player)
        }

        fn status_of(&self, suffix: &str) -> String {
            let name = format!("org.mpris.MediaPlayer2.{suffix}");
            self.players.borrow()[&name].status.clone()
        }

        fn pause_calls(&self) -> Vec<String> {
            self.paused.borrow().clone()
        }

        fn play_calls(&self) -> Vec<String> {
            self.played.borrow().clone()
        }
    }

    impl Source for FakeSource {
        fn players(&self) -> Vec<Player> {
            self.players.borrow().values().cloned().collect()
        }

        fn get(&self, name: &str) -> Option<Player> {
            self.players.borrow().get(name).cloned()
        }

        fn pause(&self, name: &str) -> Result<(), String> {
            let mut players = self.players.borrow_mut();
            let player = players.get_mut(name).ok_or("unknown player")?;
            player.status = "Paused".to_string();
            self.paused.borrow_mut().push(name.to_string());
            Ok(())
        }

        fn play(&self, name: &str) -> Result<(), String> {
            let mut players = self.players.borrow_mut();
            let player = players.get_mut(name).ok_or("unknown player")?;
            player.status = "Playing".to_string();
            self.played.borrow_mut().push(name.to_string());
            Ok(())
        }
    }

    fn manager(triggers: &[&str]) -> FocusManager {
        let mut cfg = Config::default();
        cfg.trigger_players = triggers.iter().map(|item| item.to_string()).collect();
        FocusManager::new(cfg)
    }

    #[test]
    fn new_player_takes_focus_and_pauses_others() {
        let src = FakeSource::new(&[("alpha", "Alpha", "Playing"), ("beta", "Beta", "Paused")]);
        let now = Instant::now();
        let mut mgr = manager(&[]);
        mgr.adopt(&src, now);
        assert_eq!(src.pause_calls().len(), 0, "adoption must not pause");

        src.set_status("beta", "Playing");
        mgr.on_event(src.event("beta"), &src, now);

        let status = mgr.status(&src, now);
        assert_eq!(
            status.focus.as_ref().unwrap().owner,
            "org.mpris.MediaPlayer2.beta"
        );
        assert_eq!(src.status_of("alpha"), "Paused");
        assert_eq!(src.pause_calls(), vec!["org.mpris.MediaPlayer2.alpha"]);
    }

    #[test]
    fn trigger_player_pauses_until_it_stops_then_resumes() {
        let src = FakeSource::new(&[("music", "Music", "Playing"), ("alarm", "Alarm", "Paused")]);
        let now = Instant::now();
        let mut mgr = manager(&["alarm"]);
        mgr.adopt(&src, now);

        src.set_status("alarm", "Playing");
        mgr.on_event(src.event("alarm"), &src, now);

        let status = mgr.status(&src, now);
        let focus = status.focus.as_ref().expect("trigger focus");
        assert_eq!(focus.owner, "org.mpris.MediaPlayer2.alarm");
        assert_eq!(focus.mode, "transient");
        assert!(focus.locked);
        assert_eq!(src.status_of("music"), "Paused");

        src.set_status("alarm", "Stopped");
        mgr.on_event(src.event("alarm"), &src, now + Duration::from_secs(5));

        let status = mgr.status(&src, now + Duration::from_secs(5));
        assert_eq!(
            status.focus.as_ref().unwrap().owner,
            "org.mpris.MediaPlayer2.music"
        );
        assert_eq!(src.status_of("music"), "Playing");
        assert_eq!(src.play_calls(), vec!["org.mpris.MediaPlayer2.music"]);
    }

    #[test]
    fn manual_trigger_pauses_new_players_until_it_ends() {
        let src = FakeSource::new(&[("alpha", "Alpha", "Playing"), ("beta", "Beta", "Paused")]);
        let now = Instant::now();
        let mut mgr = manager(&[]);
        mgr.adopt(&src, now);

        let req = Request {
            cmd: "trigger_start".to_string(),
            name: Some("call".to_string()),
            timeout_ms: None,
        };
        let resp = mgr.handle(&req, &src, now);
        assert!(resp.ok, "{:?}", resp.error);
        assert_eq!(src.status_of("alpha"), "Paused");

        src.set_status("beta", "Playing");
        mgr.on_event(src.event("beta"), &src, now);
        assert_eq!(src.status_of("beta"), "Paused");
        assert_eq!(mgr.status(&src, now).focus.unwrap().owner, "call");

        let req = Request {
            cmd: "trigger_end".to_string(),
            name: Some("call".to_string()),
            timeout_ms: None,
        };
        let resp = mgr.handle(&req, &src, now);
        assert!(resp.ok, "{:?}", resp.error);
        assert_eq!(src.status_of("alpha"), "Playing");
        assert_eq!(src.status_of("beta"), "Playing");
    }

    #[test]
    fn trigger_timeout_releases_and_resumes() {
        let src = FakeSource::new(&[("alpha", "Alpha", "Playing")]);
        let now = Instant::now();
        let mut mgr = manager(&[]);
        mgr.adopt(&src, now);

        let req = Request {
            cmd: "trigger_start".to_string(),
            name: Some("call".to_string()),
            timeout_ms: Some(40),
        };
        assert!(mgr.handle(&req, &src, now).ok);
        assert_eq!(src.status_of("alpha"), "Paused");
        assert!(mgr.time_until_deadline(now).is_some());

        mgr.tick(&src, now + Duration::from_millis(50));
        assert_eq!(src.status_of("alpha"), "Playing");
        assert_eq!(
            mgr.status(&src, now + Duration::from_millis(50))
                .focus
                .unwrap()
                .owner,
            "org.mpris.MediaPlayer2.alpha"
        );
    }

    #[test]
    fn exclusive_release_does_not_resume() {
        let src = FakeSource::new(&[("alpha", "Alpha", "Playing")]);
        let now = Instant::now();
        let mut mgr = manager(&[]);
        mgr.adopt(&src, now);

        src.set_status("alpha", "Paused");
        mgr.on_event(src.event("alpha"), &src, now);

        assert!(mgr.status(&src, now).focus.is_none());
        assert_eq!(src.status_of("alpha"), "Paused");
        assert!(src.play_calls().is_empty());
    }

    #[test]
    fn active_trigger_at_startup_pauses_others() {
        let src = FakeSource::new(&[("music", "Music", "Playing"), ("alarm", "Alarm", "Playing")]);
        let now = Instant::now();
        let mut mgr = manager(&["alarm"]);
        mgr.adopt(&src, now);

        let status = mgr.status(&src, now);
        let focus = status.focus.as_ref().expect("startup trigger focus");
        assert_eq!(focus.owner, "org.mpris.MediaPlayer2.alarm");
        assert!(focus.locked);
        assert_eq!(src.status_of("music"), "Paused");
    }

    #[test]
    fn paused_by_service_list_is_sorted() {
        let src = FakeSource::new(&[("alpha", "Alpha", "Playing"), ("beta", "Beta", "Playing")]);
        let now = Instant::now();
        let mut mgr = manager(&[]);

        let req = Request {
            cmd: "trigger_start".to_string(),
            name: Some("call".to_string()),
            timeout_ms: None,
        };
        assert!(mgr.handle(&req, &src, now).ok);

        let status = mgr.status(&src, now);
        assert_eq!(
            status.paused_by_service,
            vec![
                "org.mpris.MediaPlayer2.alpha".to_string(),
                "org.mpris.MediaPlayer2.beta".to_string()
            ]
        );
        assert!(status.players.iter().all(|player| player.paused_by_service));
    }

    #[test]
    fn trigger_end_rejects_wrong_name() {
        let src = FakeSource::new(&[("alpha", "Alpha", "Playing")]);
        let now = Instant::now();
        let mut mgr = manager(&[]);
        mgr.adopt(&src, now);

        let start = Request {
            cmd: "trigger_start".to_string(),
            name: Some("call".to_string()),
            timeout_ms: None,
        };
        assert!(mgr.handle(&start, &src, now).ok);

        let end = Request {
            cmd: "trigger_end".to_string(),
            name: Some("other".to_string()),
            timeout_ms: None,
        };
        let resp = mgr.handle(&end, &src, now);
        assert!(!resp.ok);
        assert!(resp.error.unwrap().contains("active trigger"));
    }

    #[test]
    fn trigger_start_is_rejected_while_locked() {
        let src = FakeSource::new(&[("alpha", "Alpha", "Playing")]);
        let now = Instant::now();
        let mut mgr = manager(&[]);
        mgr.adopt(&src, now);

        let first = Request {
            cmd: "trigger_start".to_string(),
            name: Some("call".to_string()),
            timeout_ms: None,
        };
        assert!(mgr.handle(&first, &src, now).ok);

        let second = Request {
            cmd: "trigger_start".to_string(),
            name: Some("alarm".to_string()),
            timeout_ms: None,
        };
        assert!(!mgr.handle(&second, &src, now).ok);
    }

    #[test]
    fn unknown_command_is_rejected() {
        let src = FakeSource::new(&[]);
        let now = Instant::now();
        let mut mgr = manager(&[]);
        let req = Request {
            cmd: "nope".to_string(),
            name: None,
            timeout_ms: None,
        };
        assert!(!mgr.handle(&req, &src, now).ok);
    }
}
