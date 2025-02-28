// SPDX-FileCopyrightText: 2021 Softbear, Inc.
// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::animation::Animation;
use crate::interpolated_contact::InterpolatedContact;
use crate::trail::TrailSystem;
use client_util::apply::Apply;
use common::contact::Contact;
use common::death_reason::DeathReason;
use common::entity::EntityId;
use common::protocol::Update;
use common::terrain::Terrain;
use std::collections::HashMap;

/// State associated with game server connection. Reset when connection is reset.
pub struct Mk48State {
    pub animations: Vec<Animation>,
    pub contacts: HashMap<EntityId, InterpolatedContact>,
    pub death_reason: Option<DeathReason>,
    pub entity_id: Option<EntityId>,
    pub score: u32,
    pub terrain: Terrain,
    pub trails: TrailSystem,
    pub world_radius: f32,
}

impl Default for Mk48State {
    fn default() -> Self {
        Self {
            animations: Vec::new(),
            contacts: HashMap::new(),
            death_reason: None,
            entity_id: None,
            score: 0,
            terrain: Terrain::default(),
            trails: TrailSystem::default(),
            // Keep border off splash screen by assuming radius.
            world_radius: 10000.0,
        }
    }
}

impl Mk48State {
    /// Returns the "view" of the player's boat's contact, if the player has a boat.
    pub(crate) fn player_contact(&self) -> Option<&Contact> {
        self.entity_id
            .map(|id| &self.contacts.get(&id).unwrap().view)
    }
}

impl Apply<Update> for Mk48State {
    fn apply(&mut self, update: Update) {
        self.death_reason = update.death_reason;
        self.terrain.apply_update(&update.terrain);
        self.world_radius = update.world_radius;
        self.score = update.score;
    }
}
