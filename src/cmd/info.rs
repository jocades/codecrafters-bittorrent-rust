use std::{fs, path::PathBuf};

use clap::Args;

use crate::Torrent;

#[derive(Args)]
pub struct Info {
    path: PathBuf,
}

impl Info {
    pub fn execute(&self) -> crate::Result<()> {
        let bytes = fs::read(&self.path)?;
        let torrent: Torrent = serde_bencode::from_bytes(&bytes)?;
        println!("Tracker URL: {}", torrent.announce);
        println!("Length: {}", torrent.info.length);
        Ok(())
    }
}
