// SPDX-FileCopyrightText: 2021 Softbear, Inc.
// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::entities::EntityIndex;
use crate::entity::Entity;
use crate::server::Server;
use crate::world::World;
use common::altitude::Altitude;
use common::angle::Angle;
use common::death_reason::DeathReason;
use common::entity::*;
use common::guidance::Guidance;
use common::ticks::Ticks;
use common::util::*;
use common::velocity::Velocity;
use game_server::context::PlayerTuple;
use glam::Vec2;
use rand::{thread_rng, Rng};
use std::sync::Arc;

/// Serialized mutations, targeted at an indexed entity, ordered by priority.
#[derive(Clone, Debug)]
pub(crate) enum Mutation {
    CollidedWithBoat {
        other_player: Arc<PlayerTuple<Server>>,
        damage: Ticks,
        impulse: Velocity,
        ram: bool,
    },
    CollidedWithObstacle {
        impulse: Velocity,
        entity_type: EntityType,
    },
    ClearSpawnProtection,
    UpgradeHq,
    #[allow(dead_code)]
    Score(u32),
    Remove(DeathReason),
    Repair(Ticks),
    Reload(Ticks),
    ReloadLimited {
        entity_type: EntityType,
        instant: bool,
    },
    // For things that may only be collected once.
    CollectedBy(Arc<PlayerTuple<Server>>, u32),
    HitBy(Arc<PlayerTuple<Server>>, EntityType, Ticks),
    Attraction(Vec2, Velocity),
    Guidance {
        direction_target: Angle,
        altitude_target: Altitude,
        signal_strength: f32,
    },
    FireAll(EntitySubKind),
}

impl Mutation {
    /// absolute_priority returns the priority of this mutation, higher means higher priority (going first).
    pub fn absolute_priority(&self) -> i8 {
        match self {
            Self::FireAll(_) => 127, // so that ASROC can fire before expiring
            Self::Remove(_) => 126,
            Self::HitBy(_, _, _) => 125,
            Self::CollidedWithBoat { .. } => 124,
            Self::CollectedBy(_, _) => 123,
            Self::Attraction(_, _) => 101,
            Self::Guidance { .. } => 100,
            _ => 0,
        }
    }

    /// relative_priority returns the priority of this mutation, relative to other mutations of the same absolute priority.
    /// In order for a mutation type to have relative priority relative to other mutations of the same type, it must have a unique absolute priority.
    /// Higher relative priority goes first.
    pub fn relative_priority(&self) -> f32 {
        match self {
            // If you die from two different things simultaneously, prioritize giving another player your points.
            Self::Remove(death_reason) => {
                if death_reason.is_due_to_player() {
                    1.0
                } else {
                    0.0
                }
            }
            // The last guidance (highest signal strength) is the one that will take effect.
            Self::Guidance {
                signal_strength, ..
            } => -signal_strength,
            // Highest damage goes first.
            Self::HitBy(_, _, damage) => damage.to_secs(),
            Self::CollidedWithBoat { damage, .. } => damage.to_secs(),
            // Closest attraction goes last (takes effect).
            Self::Attraction(delta, _) => delta.length_squared(),
            _ => 0.0,
        }
    }

    /// apply applies the Mutation and returns if the entity was removed.
    /// is_last_of_type is true iff this mutation is the last of its type for this entity index.
    pub fn apply(
        self,
        world: &mut World,
        index: EntityIndex,
        delta: Ticks,
        is_last_of_type: bool,
    ) -> bool {
        let entities = &mut world.entities;
        match self {
            Self::Remove(reason) => {
                #[cfg(debug_assertions)]
                {
                    let e = &entities[index];
                    if e.is_boat() {
                        Self::boat_died(world, index, false);
                        if let DeathReason::Debug(msg) = reason {
                            panic!("boat removed with debug reason {}", msg);
                        }
                    }
                }

                world.remove(index, reason);
                return true;
            }
            Self::HitBy(other_player, weapon_type, damage) => {
                let e = &mut entities[index];
                if e.damage(damage) {
                    let player_id = {
                        let mut other_player = other_player.borrow_player_mut();
                        other_player.score += kill_score(e.borrow_player().score);
                        let player_id = other_player.player_id;
                        drop(other_player);
                        player_id
                    };

                    Self::boat_died(world, index, false);
                    world.remove(index, DeathReason::Weapon(player_id, weapon_type));

                    return true;
                }
            }
            Self::CollidedWithBoat {
                damage,
                impulse,
                other_player,
                ram,
            } => {
                let entity = &mut entities[index];
                if entity.damage(damage) {
                    let player_id = {
                        let mut other_player = other_player.borrow_player_mut();
                        other_player.score += ram_score(entity.borrow_player().score);
                        let player_id = other_player.player_id;
                        drop(other_player);
                        player_id
                    };

                    Self::boat_died(world, index, false);
                    world.remove(
                        index,
                        if ram {
                            DeathReason::Ram(player_id)
                        } else {
                            DeathReason::Boat(player_id)
                        },
                    );
                    return true;
                }
                entity.transform.velocity =
                    (entity.transform.velocity + impulse).clamp_magnitude(Velocity::from_mps(15.0));
            }
            Self::CollidedWithObstacle {
                impulse,
                entity_type,
            } => {
                let entity = &mut entities[index];
                if entity.kill_in(delta, Ticks::from_secs(6.0)) {
                    Self::boat_died(world, index, true);
                    world.remove(index, DeathReason::Entity(entity_type));
                    return true;
                }
                entity.transform.velocity =
                    (entity.transform.velocity + impulse).clamp_magnitude(Velocity::from_mps(20.0));
            }
            Self::ClearSpawnProtection => entities[index].extension_mut().clear_spawn_protection(),
            Self::UpgradeHq => {
                let entity = &mut entities[index];
                entity.change_entity_type(EntityType::Hq, &mut world.arena);
                entity.ticks = Ticks::ZERO;
            }
            Self::Repair(amount) => {
                entities[index].repair(amount);
            }
            Self::Reload(amount) => {
                entities[index].reload(amount);
            }
            Self::ReloadLimited {
                entity_type,
                instant,
            } => {
                Self::reload_limited_armament(world, index, entity_type, instant);
            }
            Self::Score(score) => {
                entities[index].borrow_player_mut().score += score;
            }
            Self::CollectedBy(player, score) => {
                player.borrow_player_mut().score += score;
                world.remove(index, DeathReason::Unknown);
                return true;
            }
            Self::Guidance {
                direction_target,
                altitude_target,
                ..
            } => {
                // apply_altitude_target is not reversed by another Guidance mutation, so must
                // be sure to only apply one Guidance mutation.
                if is_last_of_type {
                    let entity = &mut entities[index];
                    entity.guidance.direction_target = direction_target;
                    entity.apply_altitude_target(&world.terrain, Some(altitude_target), 5.0, delta);
                }
            }
            Self::Attraction(delta, velocity) => {
                let transform = &mut entities[index].transform;
                transform.direction = Angle::from(delta);
                transform.velocity = velocity;
            }
            Self::FireAll(sub_kind) => {
                let entity = &mut entities[index];

                // Reset entity lifespan (because it is actively engaging in battle.
                entity.ticks = Ticks::ZERO;

                let data = entity.data();
                let armament_entities: Vec<Entity> = data
                    .armaments
                    .iter()
                    .enumerate()
                    .filter_map(|(i, armament)| {
                        let armament_data: &EntityData = armament.entity_type.data();
                        if armament_data.sub_kind == sub_kind {
                            let mut armament_entity = Entity::new(
                                armament.entity_type,
                                Some(Arc::clone(entity.player.as_ref().unwrap())),
                            );

                            armament_entity.ticks =
                                armament.entity_type.reduced_lifespan(Ticks::from_secs(
                                    150.0 / armament_data.speed.to_mps().clamp(15.0, 50.0),
                                ));
                            armament_entity.transform =
                                entity.transform + data.armament_transform(&[], i);
                            armament_entity.altitude = entity.altitude;
                            armament_entity.guidance = Guidance {
                                direction_target: entity.transform.direction, // TODO: Randomize
                                velocity_target: armament.entity_type.data().speed,
                            };

                            // Max drop velocity.
                            armament_entity.transform.velocity = armament_entity
                                .transform
                                .velocity
                                .clamp_magnitude(Velocity::from_mps(50.0));

                            Some(armament_entity)
                        } else {
                            None
                        }
                    })
                    .collect();

                // Cannot spawn in loop that borrows entity's guidance.
                for armament_entity in armament_entities {
                    world.spawn_here_or_nearby(armament_entity, 0.0, None);
                }
            }
        };
        false
    }

    /// boat_died applies the effect of a boat dying, such as a reduction in the corresponding player's
    /// score and the spawning of loot.
    ///
    /// If killed by a player, that player will get the coins. If killed by land or by fleeing combat,
    /// score should be converted into coins to strategic destruction of score.
    pub fn boat_died(world: &mut World, index: EntityIndex, score_to_coins: bool) {
        let entity = &mut world.entities[index];
        let mut player = entity.borrow_player_mut();
        let score = player.score;
        player.score = respawn_score(player.score);
        drop(player);

        let data = entity.data();
        debug_assert_eq!(data.kind, EntityKind::Boat);
        let mut rng = thread_rng();
        // Loot is based on the length of the boat.

        let center = entity.transform.position;
        let normal = entity.transform.direction.to_vec();
        let tangent = Vec2::new(-normal.y, normal.x);
        let altitude = entity.altitude;

        for loot_type in entity.entity_type.loot(score, score_to_coins) {
            let mut loot_entity = Entity::new(loot_type, None);

            // Make loot roughly conform to rectangle of ship.
            loot_entity.transform.position = center
                + normal * (rng.gen::<f32>() - 0.5) * data.length
                + tangent * (rng.gen::<f32>() - 0.5) * data.width;
            loot_entity.altitude = altitude;

            // Randomize lifespan a bit to avoid all spawned entities dying at the same time.
            let lifespan = loot_type.data().lifespan;
            if lifespan != Ticks::ZERO {
                loot_entity.ticks += lifespan * (rng.gen::<f32>() * 0.25)
            }

            world.spawn_here_or_nearby(loot_entity, data.radius * 0.15, None);
        }
    }

    /// Call when a weapon, decoy, or aircraft dies and the player may still be alive, so it may be
    /// reloaded if it is a limited armament.
    pub fn reload_limited_armament(
        world: &mut World,
        boat_index: EntityIndex,
        entity_type: EntityType,
        instant: bool,
    ) {
        let limited_data: &EntityData = entity_type.data();

        if !limited_data.limited {
            // Not a limited armament.
            return;
        }

        debug_assert!(
            limited_data.kind == EntityKind::Weapon
                || limited_data.kind == EntityKind::Aircraft
                || limited_data.kind == EntityKind::Decoy
        );

        let boat = &mut world.entities[boat_index];
        let data = boat.data();
        let extension = boat.extension_mut();
        let consumption = extension.reloads_mut();

        for (i, armament) in data.armaments.iter().enumerate() {
            if armament.entity_type != entity_type || consumption[i] != Ticks::MAX {
                continue;
            }

            consumption[i] = if instant {
                Ticks::ZERO
            } else {
                armament.reload()
            };

            return;
        }
    }
}
