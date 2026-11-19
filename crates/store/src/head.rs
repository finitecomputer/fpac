//! The head: which checkpoint a disk opens from, and which life's
//! segments follow it (docs/design.md, Consistency). The platform keeps
//! it, as typed columns, and moves it only by these rules, which are pure
//! so the platform's route and this crate's tests share them.

use fpac_format::Sum;

/// A checkpoint by its name's parts and its object's SHA-256.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckpointRef {
    pub epoch: u64,
    pub seq: u64,
    pub sum: Sum,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Head {
    /// The life whose segments follow the checkpoint. A life posts its
    /// head before it ships anything, so a life that never posted one
    /// shipped nothing, and an open replays one life's segments only.
    pub epoch: u64,
    /// None: a new disk, with nothing before this life's segments.
    pub checkpoint: Option<CheckpointRef>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    Take,
    /// The same head again: an earlier post whose answer was lost.
    Same,
    Refuse(&'static str),
}

/// Whether the platform takes `proposed` over `current`, from the life
/// `running` (the start generation it minted). Only the running life may
/// move the head; within a life the checkpoint only moves forward; a
/// new life may name any checkpoint (it opened from the old head, and is
/// the only writer).
pub fn decide(current: Option<&Head>, running: u64, proposed: &Head) -> Decision {
    if proposed.epoch != running {
        return Decision::Refuse("only the running life posts a head");
    }
    if proposed.checkpoint.is_some_and(|c| c.epoch > proposed.epoch) {
        return Decision::Refuse("a checkpoint from a later life");
    }
    let Some(cur) = current else { return Decision::Take };
    if cur == proposed {
        return Decision::Same;
    }
    if cur.epoch > proposed.epoch {
        return Decision::Refuse("an earlier life's head");
    }
    if cur.epoch < proposed.epoch {
        return Decision::Take;
    }
    // The same life: a later checkpoint of its own, only.
    match (cur.checkpoint, proposed.checkpoint) {
        (_, None) => Decision::Refuse("a life's head loses its checkpoint"),
        (None, Some(p)) => {
            if p.epoch == proposed.epoch {
                Decision::Take
            } else {
                Decision::Refuse("a life's head goes back to another life's checkpoint")
            }
        }
        (Some(c), Some(p)) => {
            if p.epoch != proposed.epoch && p != c {
                Decision::Refuse("a life's head goes back to another life's checkpoint")
            } else if p.epoch == c.epoch && p.seq == c.seq {
                Decision::Refuse("the same checkpoint with another body")
            } else if p.epoch == c.epoch && p.seq < c.seq {
                Decision::Refuse("a checkpoint older than the head's")
            } else {
                Decision::Take
            }
        }
    }
}
