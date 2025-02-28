// SPDX-FileCopyrightText: 2021 Softbear, Inc.
// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::entity::{EntityKind, EntityType};
use core_protocol::id::*;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

// DeathReason stores what a player collided with in order to die.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DeathReason {
    // For non-boats, and for when boats are removed without a better reason.
    Unknown,
    Border,
    Terrain,
    Boat(PlayerId),
    // Applies to hitting stuff like structures, never boats.
    Entity(EntityType),
    Ram(PlayerId),
    Weapon(PlayerId, EntityType),
    // Allows code to convey a reason for killing an entity that is not necessarily a player's boat.
    // In release mode, Unknown is used instead.
    #[cfg(debug_assertions)]
    Debug(String),
}

impl DeathReason {
    /// is_due_to_player returns whether the death was caused by another player, as opposed to
    /// natural causes.
    pub fn is_due_to_player(&self) -> bool {
        match self {
            Self::Unknown => false,
            Self::Border => false,
            Self::Terrain => false,
            Self::Boat(_) => true,
            Self::Entity(entity_type) => {
                // The assumption here is that all boats are controlled by players, and therefore
                // should kill via Self::Boat not Self::Entity.
                debug_assert!(entity_type.data().kind != EntityKind::Boat);
                false
            }
            Self::Ram(_) => true,
            Self::Weapon(_, _) => true,
            #[cfg(debug_assertions)]
            Self::Debug(_) => false,
        }
    }
}

impl PartialOrd for DeathReason {
    fn partial_cmp(&self, _: &Self) -> Option<Ordering> {
        // All deaths are created equal (for the purposes of sorting mutations).
        Some(Ordering::Equal)
    }
}

impl Ord for DeathReason {
    fn cmp(&self, _: &Self) -> Ordering {
        // All deaths are created equal (for the purposes of sorting mutations).
        Ordering::Equal
    }
}
