use crate::actors::api::{LookupServer, NotifyAccountState};
use crate::database::{Database, VerificationOutcome};
use crate::primitives::{ExternalMessage, IdentityContext};
use crate::primitives::{JudgementState, NotificationMessage};
use crate::Result;
use actix::prelude::*;
use actix_broker::{Broker, SystemBroker};
use std::collections::HashMap;
use tokio::time::{interval, Duration};

#[derive(Clone)]
pub struct SessionNotifier {
    db: Database,
    server: Addr<LookupServer>,
}

impl SessionNotifier {
    pub fn new(db: Database, server: Addr<LookupServer>) -> Self {
        SessionNotifier {
            db: db,
            server: server,
        }
    }
    pub async fn start(self) {
        let mut interval = interval(Duration::from_secs(1));

        let (mut db, server) = (self.db, self.server);
        tokio::spawn(async move {
            loop {
                interval.tick().await;

                match db.fetch_events().await {
                    Ok(events) => {
                        let mut cache: HashMap<IdentityContext, JudgementState> = HashMap::new();

                        for event in events {
                            let state = match cache.get(event.context()) {
                                Some(state) => state.clone(),
                                None => {
                                    let state = db
                                        .fetch_judgement_state(event.context())
                                        .await
                                        // TODO: Handle unwrap
                                        .unwrap()
                                        .ok_or(anyhow!(
                                            "No identity state found for context: {:?}",
                                            event.context()
                                        ))
                                        .unwrap();

                                    cache.insert(event.context().clone(), state.clone());

                                    state
                                }
                            };

                            server.do_send(NotifyAccountState {
                                state: state,
                                notifications: vec![event],
                            });
                        }
                    }
                    Err(err) => {
                        error!("Error fetching events from database: {:?}", err);
                    }
                }
            }
        });
    }
}
