//! The client's sync decision at the start of a session (CLOUD_SAVES.md §9): what the launcher
//! does before it starts the game, from three facts:
//!
//! - **local**: the newest *sealed* revision on this PC (every file matches `commit`), or none;
//! - **mark**: what this PC last synced with the cloud (revision and content digest), written
//!   atomically after a confirmed upload or a verified install, never cleared on an error;
//! - **cloud**: the head returned by the server.
//!
//! Compared to Beta's start-up sync (`hook/adapters/azimuth_cloud.c:1353-1454`):
//! - an unreachable server is not an input: the caller simply does not go online, and the mark
//!   is kept, so the next start still knows what the last session based its save on (failure 1);
//! - a failed upload is never followed by a download over the local save (failure 2): the caller
//!   retries or stays offline, the decision does not change;
//! - when both sides moved, the player chooses; nothing is overwritten silently (failures 4, 7).

use crate::model::{Digest, Revision};

/// The newest sealed local save.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalSave {
    /// Its content digest ([`crate::bundle::content_digest`]).
    pub content: Digest,
}

/// What this PC last synced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SyncMark {
    /// The cloud revision the local save was equal to.
    pub revision: Revision,
    /// That revision's content digest.
    pub content: Digest,
}

/// The cloud head as the server reported it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CloudHead {
    /// Head revision ([`Revision::NONE`] when the line is empty).
    pub revision: Revision,
    /// Its content digest (`None` when empty).
    pub content: Option<Digest>,
}

/// What to do before starting the game.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// No save anywhere (a Noob profile before its first upload): start the game; its first
    /// sealed revision becomes the first upload.
    Empty,
    /// Local equals cloud. Update the mark if it differs, then play.
    InSync {
        /// The head.
        revision: Revision,
    },
    /// This PC has no save for the profile: download and install the head, then play.
    Download {
        /// The head.
        revision: Revision,
    },
    /// The cloud moved and this PC did not since the mark: download and install the head.
    FastForward {
        /// The head.
        revision: Revision,
    },
    /// This PC has progress the cloud never saw and the cloud did not move since the mark:
    /// upload it with `If-Match: expected` (a crashed or offline end of the last session).
    Upload {
        /// The head the upload is based on.
        expected: Revision,
    },
    /// The cloud is empty and this PC has a save: the profile's first save (Noob); a Hardcore
    /// line is never empty once created (the server writes the fresh career).
    FirstUpload,
    /// Both moved since the mark, or there is no mark and they differ: ask the player, showing
    /// both summaries. "Keep cloud" parks this PC's save and downloads the head; "keep this PC"
    /// uploads with `If-Match: cloud` and resolution `keep_local` (the head stays in history).
    Conflict {
        /// The head.
        cloud: Revision,
        /// The mark's revision, when there is one.
        base: Option<Revision>,
    },
}

/// The decision table.
pub fn decide(local: Option<LocalSave>, mark: Option<SyncMark>, cloud: CloudHead) -> Decision {
    let (Some(cloud_content), false) = (cloud.content, cloud.revision.is_none()) else {
        return match (local, mark) {
            (None, _) => Decision::Empty,
            // A mark on an empty line means the line was emptied (profile erased and recreated):
            // never upload over that without the player.
            (Some(_), Some(m)) if !m.revision.is_none() => Decision::Conflict {
                cloud: cloud.revision,
                base: Some(m.revision),
            },
            (Some(_), _) => Decision::FirstUpload,
        };
    };
    let Some(local) = local else {
        return Decision::Download {
            revision: cloud.revision,
        };
    };
    if local.content == cloud_content {
        return Decision::InSync {
            revision: cloud.revision,
        };
    }
    match mark {
        Some(m) if m.revision == cloud.revision && m.content == cloud_content => Decision::Upload {
            expected: cloud.revision,
        },
        Some(m) if m.revision < cloud.revision && m.content == local.content => {
            Decision::FastForward {
                revision: cloud.revision,
            }
        }
        Some(m) => Decision::Conflict {
            cloud: cloud.revision,
            base: Some(m.revision),
        },
        None => Decision::Conflict {
            cloud: cloud.revision,
            base: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(tag: &[u8]) -> Digest {
        Digest::of(tag)
    }

    fn head(rev: u64, content: &[u8]) -> CloudHead {
        CloudHead {
            revision: Revision::new(rev),
            content: Some(d(content)),
        }
    }

    const EMPTY: CloudHead = CloudHead {
        revision: Revision::NONE,
        content: None,
    };

    fn local(content: &[u8]) -> Option<LocalSave> {
        Some(LocalSave {
            content: d(content),
        })
    }

    fn mark(rev: u64, content: &[u8]) -> Option<SyncMark> {
        Some(SyncMark {
            revision: Revision::new(rev),
            content: d(content),
        })
    }

    #[test]
    fn empty_lines() {
        assert_eq!(decide(None, None, EMPTY), Decision::Empty);
        assert_eq!(decide(local(b"a"), None, EMPTY), Decision::FirstUpload);
        assert_eq!(
            decide(local(b"a"), mark(4, b"a"), EMPTY),
            Decision::Conflict {
                cloud: Revision::NONE,
                base: Some(Revision::new(4))
            }
        );
    }

    #[test]
    fn equal_content_is_in_sync_whatever_the_mark() {
        assert_eq!(
            decide(local(b"x"), None, head(7, b"x")),
            Decision::InSync {
                revision: Revision::new(7)
            }
        );
        assert_eq!(
            decide(local(b"x"), mark(2, b"old"), head(7, b"x")),
            Decision::InSync {
                revision: Revision::new(7)
            }
        );
    }

    #[test]
    fn a_new_pc_downloads() {
        assert_eq!(
            decide(None, None, head(7, b"x")),
            Decision::Download {
                revision: Revision::new(7)
            }
        );
    }

    #[test]
    fn local_progress_on_an_unmoved_cloud_uploads() {
        // Beta failure 4: the last session's tail never reached the server; it does now.
        assert_eq!(
            decide(local(b"tail"), mark(7, b"x"), head(7, b"x")),
            Decision::Upload {
                expected: Revision::new(7)
            }
        );
    }

    #[test]
    fn cloud_progress_on_an_unchanged_pc_fast_forwards() {
        assert_eq!(
            decide(local(b"x"), mark(7, b"x"), head(9, b"y")),
            Decision::FastForward {
                revision: Revision::new(9)
            }
        );
    }

    #[test]
    fn both_moved_is_the_players_choice() {
        assert_eq!(
            decide(local(b"pc"), mark(7, b"x"), head(9, b"y")),
            Decision::Conflict {
                cloud: Revision::new(9),
                base: Some(Revision::new(7))
            }
        );
        // No mark (state file lost, first launch with an offline save): never guess.
        assert_eq!(
            decide(local(b"pc"), None, head(9, b"y")),
            Decision::Conflict {
                cloud: Revision::new(9),
                base: None
            }
        );
        // A mark that does not match the head it names (the cloud was restored): ask.
        assert_eq!(
            decide(local(b"pc"), mark(9, b"other"), head(9, b"y")),
            Decision::Conflict {
                cloud: Revision::new(9),
                base: Some(Revision::new(9))
            }
        );
    }
}
