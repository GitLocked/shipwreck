// SPDX-FileCopyrightText: 2021 Softbear, Inc.
// SPDX-License-Identifier: AGPL-3.0-or-later

#![allow(unused_imports)]
#![allow(dead_code)]

use crate::metrics::Metrics;
use crate::session::Session;
use aws_config::default_provider::credentials::DefaultCredentialsChain;
use aws_sdk_dynamodb::model::AttributeValue;
use aws_sdk_dynamodb::{Client, Region};
use core_protocol::id::*;
use core_protocol::name::*;
use core_protocol::serde_util::StrVisitor;
use core_protocol::{get_unix_time_now, UnixTime};
use serde::de::DeserializeOwned;
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use std::collections::HashMap;
use std::mem;
use variant_count::VariantCount;

/// A DynamoDB database.
pub struct Database {
    client: Client,
}

/// The type of leaderboard score, for a particular game.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash, Serialize, Deserialize, VariantCount)]
pub enum ScoreType {
    #[serde(rename = "player/all")]
    PlayerAllTime = 0,
    #[serde(rename = "player/week")]
    PlayerWeek = 1,
    #[serde(rename = "player/day")]
    PlayerDay = 2,
    #[serde(rename = "team/all")]
    TeamAllTime = 3,
    #[serde(rename = "team/week")]
    TeamWeek = 4,
    #[serde(rename = "team/day")]
    TeamDay = 5,
}

/// The type of leaderboard score, for any game. Serialized as "GameId/ScoreType".
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct GameIdScoreType {
    game_id: GameId,
    score_type: ScoreType,
}

impl Serialize for GameIdScoreType {
    fn serialize<S>(&self, serializer: S) -> Result<<S as Serializer>::Ok, <S as Serializer>::Error>
    where
        S: Serializer,
    {
        let av_game_id: AttributeValue =
            serde_dynamo::generic::to_attribute_value(self.game_id).unwrap();
        let av_game_score_type: AttributeValue =
            serde_dynamo::generic::to_attribute_value(self.score_type).unwrap();
        serializer.serialize_str(&format!(
            "{}/{}",
            av_game_id.as_s().unwrap(),
            av_game_score_type.as_s().unwrap()
        ))
    }
}

impl<'de> Deserialize<'de> for GameIdScoreType {
    fn deserialize<D>(deserializer: D) -> Result<Self, <D as Deserializer<'de>>::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_str(StrVisitor).and_then(|s| {
            let mut split = s.splitn(2, '/');
            if let Some((s_game_id, s_game_score_type)) = split.next().zip(split.next()) {
                let game_id_opt = serde_dynamo::generic::from_attribute_value(AttributeValue::S(
                    String::from(s_game_id),
                ))
                .ok();
                let game_score_type_opt = serde_dynamo::generic::from_attribute_value(
                    AttributeValue::S(String::from(s_game_score_type)),
                )
                .ok();
                return if let Some((game_id, game_score_type)) =
                    game_id_opt.zip(game_score_type_opt)
                {
                    Ok(Self {
                        game_id,
                        score_type: game_score_type,
                    })
                } else {
                    Err(de::Error::custom("parse error"))
                };
            }
            Err(de::Error::custom("wrong format"))
        })
    }
}

impl ScoreType {
    /// Returns corresponding period as unix timestamp seconds.
    fn period(self) -> Option<u64> {
        match self {
            Self::PlayerAllTime | Self::TeamAllTime => None,
            Self::PlayerWeek | Self::TeamWeek => Some(60 * 60 * 24 * 7),
            Self::PlayerDay | Self::TeamDay => Some(60 * 60 * 24),
        }
    }
}

/// A score of known score type.
#[derive(Debug, Clone)]
pub struct Score {
    pub alias: String,
    pub score: u32,
}

/// A database row storing a score.
#[derive(Serialize, Deserialize)]
struct ScoreItem {
    /// Hash key.
    game_id_score_type: GameIdScoreType,
    /// Range key.
    alias: String,
    score: u32,
    /// Unix seconds when DynamoDB should expire.
    #[serde(skip_serializing_if = "Option::is_none")]
    ttl: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SessionItem {
    pub alias: PlayerAlias,
    /// Hash key.
    pub arena_id: ArenaId,
    pub date_created: UnixTime,
    pub date_previous: Option<UnixTime>,
    pub date_renewed: UnixTime,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date_terminated: Option<UnixTime>,
    pub game_id: GameId,
    pub player_id: PlayerId,
    pub plays: u32,
    pub previous_id: Option<SessionId>,
    pub referrer: Option<Referrer>,
    pub user_agent_id: Option<UserAgentId>,
    /// Unlike RAM cache Session, not optional because storing localhost sessions in the database
    /// makes no sense.
    pub server_id: ServerId,
    /// Range key.
    pub session_id: SessionId,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct MetricsItem {
    /// Hash key.
    pub game_id: GameId,
    /// Sort key.
    pub timestamp: UnixTime,
    #[serde(flatten)]
    pub metrics: Metrics,
}

#[derive(Debug)]
pub enum Error {
    Dynamo(aws_sdk_dynamodb::Error),
    Serde(serde_dynamo::Error),
}

impl Database {
    const REGION: &'static str = "us-east-1";
    const SESSIONS_TABLE_NAME: &'static str = "core_sessions";
    const SCORES_TABLE_NAME: &'static str = "core_scores";
    const METRICS_TABLE_NAME: &'static str = "core_metrics";

    pub async fn new() -> Self {
        let credentials_provider = DefaultCredentialsChain::builder()
            .region(Region::new(Self::REGION))
            .profile_name("core")
            .build()
            .await;
        let shared_config = aws_config::from_env()
            .credentials_provider(credentials_provider)
            .region(Self::REGION)
            .load()
            .await;
        let client = Client::new(&shared_config);
        Self { client }
    }

    /// Call with current scores. Result is all leaderboards, including a prediction of how new
    /// items have affected it.
    pub async fn update_leaderboard(
        &self,
        game_id: GameId,
        player_scores: Vec<Score>,
    ) -> Result<HashMap<ScoreType, Vec<Score>>, Error> {
        // DynamoDB ttl is in seconds.
        let now = get_unix_time_now() / 1000;
        let current_scores = self.read_scores().await?;
        let mut leaderboard: HashMap<ScoreType, Vec<Score>> =
            HashMap::with_capacity(ScoreType::VARIANT_COUNT);

        // Must get at least this to be forwarded to database. Start at 1 so 0 never makes it on
        // to the leaderboard.
        let mut minimum_thresholds = [1; ScoreType::VARIANT_COUNT];

        for score in current_scores {
            if score.game_id_score_type.game_id != game_id {
                // TODO: When there are many games, more efficient to let DB handle this.
                continue;
            }

            leaderboard
                .entry(score.game_id_score_type.score_type)
                .or_insert_with(|| Vec::with_capacity(15))
                .push(Score {
                    alias: score.alias,
                    score: score.score,
                });
        }

        for (score_type, scores) in leaderboard.iter_mut() {
            scores.sort_unstable_by(|a, b| b.score.cmp(&a.score));
            // Leave a grace margin of 5, to guard against possibility of scores in the top 10 aging out.
            scores.truncate(15);
            if let Some(lowest) = scores.get(14) {
                minimum_thresholds[*score_type as usize] = lowest.score;
            }
        }

        for score in player_scores.into_iter() {
            for score_type in [
                ScoreType::PlayerDay,
                ScoreType::PlayerWeek,
                ScoreType::PlayerAllTime,
            ] {
                if score.score >= minimum_thresholds[score_type as usize] {
                    let scores = leaderboard
                        .entry(score_type)
                        .or_insert_with(|| Vec::with_capacity(10));

                    // TODO: O(n) lookup, although n is probably small.
                    if let Some(existing) = scores
                        .iter_mut()
                        .find(|existing| existing.alias == score.alias)
                    {
                        existing.score = existing.score.max(score.score);
                    } else {
                        scores.push(score.clone());
                    }

                    self.update_score(ScoreItem {
                        game_id_score_type: GameIdScoreType {
                            game_id,
                            score_type,
                        },
                        alias: score.alias.clone(),
                        score: score.score,
                        ttl: score_type.period().map(|period| now + period),
                    })
                    .await?;
                }
            }
        }

        // Produce the final leaderboard, taking into account recently updated scores (without
        // rereading them).
        for (_, scores) in leaderboard.iter_mut() {
            scores.sort_unstable_by(|a, b| b.score.cmp(&a.score));
            scores.truncate(10);
        }

        Ok(leaderboard)
    }

    async fn put<I: Serialize>(&self, item: I, table: &'static str) -> Result<(), Error> {
        let ser = match serde_dynamo::generic::to_item(item) {
            Ok(ser) => ser,
            Err(e) => return Err(Error::Serde(e)),
        };

        match self
            .client
            .put_item()
            .table_name(table)
            .set_item(Some(ser))
            .send()
            .await
        {
            Err(e) => Err(Error::Dynamo(e.into())),
            Ok(_) => Ok(()),
        }
    }

    pub async fn get2<HK: Serialize, RK: Serialize, O: DeserializeOwned>(
        &self,
        table: &'static str,
        hash_name: &'static str,
        hash_value: HK,
        range_name: &'static str,
        range_value: RK,
    ) -> Result<Option<O>, Error> {
        let hash_ser: AttributeValue = match serde_dynamo::generic::to_attribute_value(hash_value) {
            Err(e) => return Err(Error::Serde(e)),
            Ok(key_ser) => key_ser,
        };

        let range_ser: AttributeValue = match serde_dynamo::generic::to_attribute_value(range_value)
        {
            Err(e) => return Err(Error::Serde(e)),
            Ok(key_ser) => key_ser,
        };

        let mut get_item_output = match self
            .client
            .get_item()
            .table_name(table)
            .key(hash_name, hash_ser)
            .key(range_name, range_ser)
            .send()
            .await
        {
            Ok(output) => output,
            Err(e) => return Err(Error::Dynamo(e.into())),
        };

        if let Some(item) = mem::take(&mut get_item_output.item) {
            match serde_dynamo::generic::from_item(item) {
                Err(e) => Err(Error::Serde(e)),
                Ok(de) => Ok(Some(de)),
            }
        } else {
            Ok(None)
        }
    }

    async fn scan_inner<O: DeserializeOwned>(
        &self,
        table: &'static str,
        last_evaluated_key: Option<HashMap<String, AttributeValue>>,
    ) -> Result<(Vec<O>, Option<HashMap<String, AttributeValue>>), Error> {
        let scan_output = match self
            .client
            .scan()
            .table_name(table)
            .set_exclusive_start_key(last_evaluated_key)
            .send()
            .await
        {
            Ok(output) => output,
            Err(e) => return Err(Error::Dynamo(e.into())),
        };

        let mut ret = Vec::new();
        for item in scan_output.items.unwrap_or_default() {
            match serde_dynamo::generic::from_item(item) {
                Err(e) => return Err(Error::Serde(e)),
                Ok(de) => ret.push(de),
            }
        }
        Ok((ret, scan_output.last_evaluated_key))
    }

    async fn scan<O: DeserializeOwned>(&self, table: &'static str) -> Result<Vec<O>, Error> {
        let mut ret = Vec::new();
        let mut last_evaluated_key = None;
        loop {
            match self.scan_inner(table, last_evaluated_key).await {
                Err(e) => return Err(e),
                Ok((mut items, lek)) => {
                    ret.append(&mut items);
                    last_evaluated_key = lek;

                    if last_evaluated_key.is_none() {
                        break;
                    }
                }
            }
        }

        Ok(ret)
    }

    pub async fn query_inner<O: DeserializeOwned>(
        &self,
        table: &'static str,
        hash_name: &'static str,
        hash_value: AttributeValue,
        range_key_bounds: Option<(&'static str, Option<AttributeValue>, Option<AttributeValue>)>,
        last_evaluated_key: Option<HashMap<String, AttributeValue>>,
        ignore_corrupt: bool,
    ) -> Result<(Vec<O>, Option<HashMap<String, AttributeValue>>), Error> {
        let mut scan = self
            .client
            .query()
            .table_name(table)
            .expression_attribute_names("#h", hash_name)
            .expression_attribute_values(":hv", hash_value)
            .set_exclusive_start_key(last_evaluated_key);

        if let Some(key_bounds) = range_key_bounds {
            match (key_bounds.1, key_bounds.2) {
                (None, None) => scan = scan.key_condition_expression("#h = :hv"),
                (Some(lo), None) => {
                    scan = scan
                        .key_condition_expression("#h = :hv AND #r >= :lo")
                        .expression_attribute_names("#r", key_bounds.0)
                        .expression_attribute_values(":lo", lo)
                }
                (None, Some(hi)) => {
                    scan = scan
                        .key_condition_expression("#h = :hv AND #r <= hi")
                        .expression_attribute_names("#r", key_bounds.0)
                        .expression_attribute_values(":hi", hi)
                }
                (Some(lo), Some(hi)) => {
                    scan = scan
                        .key_condition_expression("#h = :hv AND #r BETWEEN :lo :hi")
                        .expression_attribute_names("#r", key_bounds.0)
                        .expression_attribute_values(":lo", lo)
                        .expression_attribute_values(":hi", hi)
                }
            }
        } else {
            scan = scan.key_condition_expression("#h = :hv");
        }

        let scan_output = match scan.send().await {
            Ok(output) => output,
            Err(e) => return Err(Error::Dynamo(e.into())),
        };

        let mut ret = Vec::new();
        for item in scan_output.items.unwrap_or_default() {
            match serde_dynamo::generic::from_item(item) {
                Err(e) => {
                    if !ignore_corrupt {
                        return Err(Error::Serde(e));
                    }
                }
                Ok(de) => ret.push(de),
            }
        }
        Ok((ret, scan_output.last_evaluated_key))
    }

    pub async fn query<HK: Serialize, O: DeserializeOwned>(
        &self,
        table: &'static str,
        hash_name: &'static str,
        hash_value: HK,
        ignore_corrupt: bool,
    ) -> Result<Vec<O>, Error> {
        let key_ser = to_av(hash_value)?;

        let mut ret = Vec::new();
        let mut last_evaluated_key = None;
        loop {
            match self
                .query_inner(
                    table,
                    hash_name,
                    key_ser.clone(),
                    None,
                    last_evaluated_key,
                    ignore_corrupt,
                )
                .await
            {
                Err(e) => return Err(e),
                Ok((mut items, lek)) => {
                    ret.append(&mut items);
                    last_evaluated_key = lek;

                    if last_evaluated_key.is_none() {
                        break;
                    }
                }
            }
        }

        Ok(ret)
    }

    pub async fn query_hash_range<HK: Serialize, RK: Serialize, O: DeserializeOwned>(
        &self,
        table: &'static str,
        hash_key: (&'static str, HK),
        range_key_bounds: (&'static str, Option<RK>, Option<RK>),
        ignore_corrupt: bool,
    ) -> Result<Vec<O>, Error> {
        let key_ser = to_av(hash_key.1)?;

        let bounds = (
            range_key_bounds.0,
            if let Some(b) = range_key_bounds.1 {
                Some(to_av(b)?)
            } else {
                None
            },
            if let Some(b) = range_key_bounds.2 {
                Some(to_av(b)?)
            } else {
                None
            },
        );

        let mut ret = Vec::new();
        let mut last_evaluated_key = None;
        loop {
            match self
                .query_inner(
                    table,
                    hash_key.0,
                    key_ser.clone(),
                    Some(bounds.clone()),
                    last_evaluated_key,
                    ignore_corrupt,
                )
                .await
            {
                Err(e) => return Err(e),
                Ok((mut items, lek)) => {
                    ret.append(&mut items);
                    last_evaluated_key = lek;

                    if last_evaluated_key.is_none() {
                        break;
                    }
                }
            }
        }

        Ok(ret)
    }

    pub async fn update_metric(&self, metric: Metrics) -> Result<(), Error> {
        self.put(metric, Self::METRICS_TABLE_NAME).await
    }

    /// Updates a score, provided that the score is actually higher.
    async fn update_score(&self, score_item: ScoreItem) -> Result<(), Error> {
        let ser = match serde_dynamo::generic::to_item(&score_item) {
            Ok(ser) => ser,
            Err(e) => return Err(Error::Serde(e)),
        };

        let ser_threshold: AttributeValue =
            match serde_dynamo::generic::to_attribute_value(score_item.score) {
                Ok(ser) => ser,
                Err(e) => return Err(Error::Serde(e)),
            };

        if let Err(e) = self
            .client
            .put_item()
            .table_name(Self::SCORES_TABLE_NAME)
            .set_item(Some(ser))
            .set_condition_expression(Some(String::from("attribute_not_exists(#s) OR #s < :s")))
            .expression_attribute_names("#s", "score")
            .expression_attribute_values(":s", ser_threshold)
            .send()
            .await
        {
            let compat = e.into();
            // Don't raise error if score wasn't high enough to persist.
            if !matches!(
                compat,
                aws_sdk_dynamodb::Error::ConditionalCheckFailedException(_)
            ) {
                return Err(Error::Dynamo(compat));
            }
        }
        Ok(())
    }

    async fn read_scores(&self) -> Result<Vec<ScoreItem>, Error> {
        self.scan(Self::SCORES_TABLE_NAME).await
    }

    async fn read_scores_by_type(
        &self,
        score_type: GameIdScoreType,
    ) -> Result<Vec<ScoreItem>, Error> {
        self.query(Self::SCORES_TABLE_NAME, "game_id_type", score_type, false)
            .await
    }

    pub async fn get_session(
        &self,
        arena_id: ArenaId,
        session_id: SessionId,
    ) -> Result<Option<SessionItem>, Error> {
        self.get2(
            Self::SESSIONS_TABLE_NAME,
            "arena_id",
            arena_id,
            "session_id",
            session_id,
        )
        .await
    }

    pub async fn put_session(&self, session: SessionItem) -> Result<(), Error> {
        self.put(session, Self::SESSIONS_TABLE_NAME).await
    }

    pub async fn get_metrics_between(
        &self,
        game_id: GameId,
        period_start: Option<UnixTime>,
        period_stop: Option<UnixTime>,
    ) -> Result<Vec<MetricsItem>, Error> {
        self.query_hash_range(
            Self::METRICS_TABLE_NAME,
            ("game_id", game_id),
            ("timestamp", period_start, period_stop),
            true,
        )
        .await
    }

    pub async fn update_metrics(&self, metrics_item: MetricsItem) -> Result<(), Error> {
        // Atomic compare and swap.
        let mut governor = 0;
        loop {
            let old: Option<MetricsItem> = match self
                .get2(
                    Self::METRICS_TABLE_NAME,
                    "game_id",
                    metrics_item.game_id,
                    "timestamp",
                    metrics_item.timestamp,
                )
                .await
            {
                Ok(val) => val,
                Err(e) => return Err(e),
            };

            let new_metrics_item = if let Some(old_metrics_item) = old.clone() {
                MetricsItem {
                    game_id: metrics_item.game_id,
                    timestamp: metrics_item.timestamp,
                    metrics: old_metrics_item.metrics + metrics_item.metrics.clone(),
                }
            } else {
                metrics_item.clone()
            };

            let ser = match serde_dynamo::generic::to_item(&new_metrics_item) {
                Ok(ser) => ser,
                Err(e) => return Err(Error::Serde(e)),
            };

            let mut request = self
                .client
                .put_item()
                .table_name(Self::METRICS_TABLE_NAME)
                .set_item(Some(ser));

            if let Some(old_metrics_item) = old {
                let old = old_metrics_item.metrics;
                // Condition is that the item wasn't changed elsewhere (all changes by servers hosting
                // arenas would increase the arenas field)
                request = request
                    .condition_expression("#arenas_cached.#total = :arenas_cached_total")
                    .expression_attribute_names("#arenas_cached", "arenas_cached")
                    .expression_attribute_names("#total", "total")
                    .expression_attribute_values(
                        ":arenas_cached_total",
                        to_av(old.arenas_cached.total)?,
                    );
            } else {
                // Condition is that the item wasn't created elsewhere.
                request = request
                    .condition_expression(
                        "attribute_not_exists(#game_id) AND attribute_not_exists(#timestamp)",
                    )
                    .expression_attribute_names("#game_id", "game_id")
                    .expression_attribute_names("#timestamp", "timestamp");
            }

            return match request.send().await {
                Err(e) => {
                    let compat = e.into();
                    if matches!(
                        compat,
                        aws_sdk_dynamodb::Error::ConditionalCheckFailedException(_)
                    ) && governor < 16
                    {
                        // Try again.
                        governor += 1;
                        continue;
                    } else {
                        Err(Error::Dynamo(compat))
                    }
                }
                Ok(_) => Ok(()),
            };
        }
    }
}

fn to_av<Tin: Serialize>(val: Tin) -> Result<AttributeValue, Error> {
    match serde_dynamo::generic::to_attribute_value(val) {
        Ok(ser) => Ok(ser),
        Err(e) => Err(Error::Serde(e)),
    }
}
