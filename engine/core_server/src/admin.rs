// SPDX-FileCopyrightText: 2021 Softbear, Inc.
// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::chat::log_chat;
use crate::core::*;
use crate::metrics::Metrics;
use crate::repo::*;
use crate::session::Session;
use actix::prelude::*;
use core_protocol::dto::MetricsDataPointDto;
use core_protocol::id::{ServerId, UserAgentId};
use core_protocol::name::Referrer;
use core_protocol::rpc::AdminUpdate::RedirectRequested;
use core_protocol::rpc::{AdminRequest, AdminUpdate};
use core_protocol::UnixTime;
use log::warn;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

const RESTART_TIMER_SECS: u64 = 15 * 60;

#[derive(Serialize, Deserialize)]
pub struct AdminState {
    pub auth: String,
}

impl AdminState {
    pub const AUTH: &'static str = include_str!("auth.txt");

    pub fn is_authentic(&self) -> bool {
        self.auth == Self::AUTH
    }
}

#[derive(Message, Serialize, Deserialize)]
#[rtype(result = "Result<AdminUpdate, &'static str>")]
pub struct ParameterizedAdminRequest {
    pub params: AdminState,
    pub request: AdminRequest,
}

impl Handler<ParameterizedAdminRequest> for Core {
    type Result = ResponseActFuture<Self, Result<AdminUpdate, &'static str>>;

    fn handle(&mut self, msg: ParameterizedAdminRequest, _ctx: &mut Self::Context) -> Self::Result {
        if !msg.params.is_authentic() {
            return Box::pin(fut::ready(Err("invalid auth")));
        }

        let request = msg.request;
        match request {
            // Handle asynchronous requests (i.e. those that access database).
            AdminRequest::RequestSeries {
                game_id,
                period_start,
                period_stop,
                resolution,
            } => Box::pin(
                async move {
                    Core::database()
                        .get_metrics_between(game_id, period_start, period_stop)
                        .await
                }
                .into_actor(self)
                .map(move |db_result, _act, _ctx| {
                    if let Ok(loaded) = db_result {
                        let series: Arc<[(UnixTime, MetricsDataPointDto)]> = loaded
                            .rchunks(resolution.map(|v| v.get() as usize).unwrap_or(1))
                            .map(|items| {
                                (
                                    items[0].timestamp,
                                    items
                                        .iter()
                                        .map(|i| i.metrics.clone())
                                        .sum::<Metrics>()
                                        .data_point(),
                                )
                            })
                            .collect();
                        let message = AdminUpdate::SeriesRequested { series };
                        Ok(message)
                    } else {
                        Err("failed to load")
                    }
                }),
            ),

            // Handle synchronous requests.
            _ => {
                let result = self.repo.handle_admin_sync(
                    request,
                    self.chat_log.as_deref(),
                    self.redirect_server_id,
                );
                Box::pin(fut::ready(result))
            }
        } // match request
    } // fn handle
}

impl Core {
    pub fn start_admin_timers(&self, ctx: &mut <Self as Actor>::Context) {
        ctx.run_interval(Duration::from_secs(RESTART_TIMER_SECS), |act, _ctx| {
            if act.repo.is_stoppable() {
                warn!("Stopping repo [dry run].");

                #[cfg(debug_assertions)]
                {
                    use std::process;
                    process::exit(0);
                }
            }
        });
    }
}

fn referrer_user_agent_id_filter(
    referrer: Option<Referrer>,
    user_agent_id: Option<UserAgentId>,
) -> impl Fn(&Session) -> bool {
    move |session: &Session| {
        if let Some(referrer) = referrer {
            if let Some(session_referrer) = session.referrer {
                if session_referrer != referrer {
                    return false;
                }
            } else {
                return false;
            }
        }
        if let Some(user_agent_id) = user_agent_id {
            if let Some(session_user_agent_id) = session.user_agent_id {
                if session_user_agent_id != user_agent_id {
                    return false;
                }
            } else {
                return false;
            }
        }

        true
    }
}

impl Repo {
    fn handle_admin_sync(
        &mut self,
        request: AdminRequest,
        chat_log: Option<&str>,
        redirect_server_id: Option<&'static AtomicU8>,
    ) -> Result<AdminUpdate, &'static str> {
        let result;
        match request {
            AdminRequest::RequestDay {
                game_id,
                referrer,
                user_agent_id,
            } => {
                let series = self.get_day(
                    game_id,
                    &referrer_user_agent_id_filter(referrer, user_agent_id),
                );
                result = Ok(AdminUpdate::DayRequested { series });
            }
            AdminRequest::RequestGames => {
                let games = self.get_game_ids();
                result = Ok(AdminUpdate::GamesRequested { games })
            }
            AdminRequest::RequestReferrers => {
                let referrers = self.get_referrers();
                result = Ok(AdminUpdate::ReferrersRequested { referrers })
            }
            AdminRequest::RequestRestart { conditions } => {
                self.set_stop_conditions(conditions);
                result = Ok(AdminUpdate::RestartRequested);
            }
            AdminRequest::RequestStatus => {
                result = Ok(AdminUpdate::StatusRequested {
                    healthy: self.health.healthy(),
                });
            }
            AdminRequest::RequestSummary {
                game_id,
                referrer,
                user_agent_id,
                period_start,
                period_stop,
            } => {
                if let Some(metrics) = self.get_metrics(
                    &game_id,
                    period_start,
                    period_stop,
                    &referrer_user_agent_id_filter(referrer, user_agent_id),
                ) {
                    result = Ok(AdminUpdate::SummaryRequested {
                        metrics: metrics.summarize(),
                    })
                } else {
                    result = Err("no summary")
                }
            }
            AdminRequest::RequestUserAgents => {
                let user_agents = self.get_user_agent_ids();
                result = Ok(AdminUpdate::UserAgentsRequested { user_agents })
            }
            AdminRequest::SendChat {
                arena_id,
                alias,
                message,
            } => {
                let mut sent = false;
                if let Some(arena_id) = arena_id {
                    sent |= self.admin_send_chat(arena_id, alias, &message);
                } else {
                    for arena_id in self.arenas.keys().cloned().collect::<Vec<_>>() {
                        sent |= self.admin_send_chat(arena_id, alias, &message);
                    }
                }

                if let Some(chat_log) = chat_log {
                    log_chat(
                        chat_log,
                        None,
                        false,
                        if sent { "ok" } else { "error" },
                        alias,
                        &message,
                    );
                }

                result = Ok(AdminUpdate::ChatSent { sent })
            }
            AdminRequest::RequestRedirect => {
                result = redirect_server_id
                    .map(|id| RedirectRequested {
                        server_id: ServerId::new(id.load(Ordering::Relaxed)),
                    })
                    .ok_or("unable to request redirect");
            }
            AdminRequest::SetRedirect { server_id } => {
                result = if let Some(redirect_server_id) = redirect_server_id {
                    redirect_server_id.store(
                        server_id.map(|id| id.0.get()).unwrap_or(0),
                        Ordering::Relaxed,
                    );
                    Ok(AdminUpdate::RedirectSet { server_id })
                } else {
                    Err("unable to set redirect")
                }
            }
            _ => result = Err("cannot process admin request synchronously"),
        }

        result
    }
}
