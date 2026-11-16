//! What the engine ranks for: this machine, detected once in the background, and the room on the
//! downloads disk.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use torrential_core::{Machine, Player, Preferences, Profile};

use crate::disk::{Disk, reserve};
use crate::env::Env;
use crate::log::log;
use crate::machine::detect;

pub struct Detector {
    machine: OnceLock<Machine>,
    downloads: PathBuf,
    disk: Arc<dyn Disk>,
    room: Arc<Mutex<Room>>,
}

/// The room on the downloads disk when last measured, and whether a thread measures it now.
#[derive(Default)]
struct Room {
    bytes: Option<u64>,
    measuring: bool,
}

impl Detector {
    pub fn start(env: &Env, downloads: PathBuf, disk: Arc<dyn Disk>) -> Arc<Detector> {
        let detector = Arc::new(Detector { machine: OnceLock::new(), downloads, disk, room: Arc::default() });
        detector.measure();
        let (env, filling) = (env.clone(), detector.clone());
        std::thread::spawn(move || match detect(&env) {
            Ok(machine) => {
                let profile = Profile { detected: Some(machine.clone()), ..Profile::default() };
                log(&format!("ranking for {}", profile.describe()));
                let _ = filling.machine.set(machine);
            }
            Err(error) => log(&format!("cannot detect this machine: {}", error.message)),
        });
        detector
    }

    #[cfg(test)]
    pub fn detected(machine: Machine, downloads: PathBuf, disk: Arc<dyn Disk>) -> Arc<Detector> {
        let room = Room { bytes: room(disk.as_ref(), &downloads), measuring: false };
        Arc::new(Detector { machine: OnceLock::from(machine), downloads, disk, room: Arc::new(Mutex::new(room)) })
    }

    /// The profile to rank for, with the room last measured. It measures again for the next one.
    pub fn profile(&self, preferences: Preferences, player: Player) -> Profile {
        self.measure();
        let room = self.room.lock().unwrap().bytes;
        Profile { detected: self.machine.get().cloned(), preferences, room, player }
    }

    /// Measures the room on a thread of its own, unless one is measuring now. The first look in
    /// ~/Downloads waits as long as the person takes to answer a macOS prompt.
    fn measure(&self) {
        let mut room = self.room.lock().unwrap();
        if room.measuring {
            return;
        }
        room.measuring = true;
        let (disk, downloads, measured) = (self.disk.clone(), self.downloads.clone(), self.room.clone());
        std::thread::spawn(move || {
            let bytes = self::room(disk.as_ref(), &downloads);
            *measured.lock().unwrap() = Room { bytes, measuring: false };
        });
    }
}

fn room(disk: &dyn Disk, downloads: &std::path::Path) -> Option<u64> {
    disk.space(downloads).ok().map(|space| space.free.saturating_sub(reserve(space.total)))
}
