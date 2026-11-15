//! What the engine ranks for: this machine, detected once in the background, and the room on the
//! downloads disk.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use torrential_core::{Machine, Player, Preferences, Profile};

use crate::disk::{Disk, reserve};
use crate::env::Env;
use crate::log::log;
use crate::machine::detect;

pub struct Detector {
    machine: OnceLock<Machine>,
    downloads: PathBuf,
    disk: Box<dyn Disk + Sync>,
}

impl Detector {
    pub fn start(env: &Env, downloads: PathBuf, disk: Box<dyn Disk + Sync>) -> Arc<Detector> {
        let detector = Arc::new(Detector { machine: OnceLock::new(), downloads, disk });
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
    pub fn detected(machine: Machine, downloads: PathBuf, disk: Box<dyn Disk + Sync>) -> Arc<Detector> {
        Arc::new(Detector { machine: OnceLock::from(machine), downloads, disk })
    }

    pub fn profile(&self, preferences: Preferences, player: Player) -> Profile {
        let room = self.disk.space(&self.downloads).ok().map(|space| space.free.saturating_sub(reserve(space.total)));
        Profile { detected: self.machine.get().cloned(), preferences, room, player }
    }
}
