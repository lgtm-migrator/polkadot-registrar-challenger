use crate::primitives::{
    ChainAddress, Event, ExternalMessage, IdentityContext, IdentityField, JudgementState,
    MessageId, NotificationMessage, Timestamp,
};
use crate::Result;
use bson::{doc, from_document, to_bson, to_document, Bson, Document};
use futures::StreamExt;
use mongodb::{options::UpdateOptions, Client, Database as MongoDb};
use serde::Serialize;
use std::collections::HashSet;

const IDENTITY_COLLECTION: &'static str = "identities";
const EVENT_COLLECTION: &'static str = "event_log";

/// Convenience trait. Converts a value to BSON.
trait ToBson {
    fn to_bson(&self) -> Result<Bson>;
    fn to_document(&self) -> Result<Document>;
}

impl<T: Serialize> ToBson for T {
    fn to_bson(&self) -> Result<Bson> {
        Ok(to_bson(self)?)
    }
    fn to_document(&self) -> Result<Document> {
        Ok(to_document(self)?)
    }
}

#[derive(Debug)]
pub enum VerificationOutcome {
    AlreadyVerified,
    Valid {
        state: JudgementState,
        notifications: Vec<NotificationMessage>,
    },
    Invalid {
        state: JudgementState,
        notifications: Vec<NotificationMessage>,
    },
    // TODO: Docs clarify
    SecondChallengeExpected {
        state: JudgementState,
        notifications: Vec<NotificationMessage>,
    },
    NotFound,
}

#[derive(Debug, Clone)]
pub struct Database {
    db: MongoDb,
    // TODO: This should be tracked in storage.
    event_counter: i64,
}

impl Database {
    pub async fn new(uri: &str, db: &str) -> Result<Self> {
        Ok(Database {
            db: Client::with_uri_str(uri).await?.database(db),
            event_counter: Timestamp::now().raw(),
        })
    }
    pub async fn add_judgement_request(&self, request: JudgementState) -> Result<()> {
        let coll = self.db.collection(IDENTITY_COLLECTION);

        // Check if a request of the same address exists yet (occurs when a
        // field gets updated during pending judgement process).
        let doc = coll
            .find_one(
                doc! {
                    "context": request.context.to_bson()?,
                },
                None,
            )
            .await?;

        // If it does exist, only update specific fields.
        if let Some(doc) = doc {
            let mut current: JudgementState = from_document(doc)?;

            // Determine which fields should be updated.
            let mut to_add = vec![];
            for new_field in request.fields {
                // If the current field value is the same as the new one, insert
                // the current field state back into storage. If the value is
                // new, insert/update the current field state.
                if let Some(current_field) = current
                    .fields
                    .iter()
                    .find(|current| current.value == new_field.value)
                {
                    to_add.push(current_field.clone());
                } else {
                    to_add.push(new_field);
                }
            }

            // Check verification status.
            current.is_fully_verified = if !to_add.is_empty() {
                // (Re-)set validity if new fields are available.
                false
            } else {
                current.is_fully_verified
            };

            // Set new fields.
            current.fields = to_add;

            // Update the final value in database.
            coll.update_one(
                doc! {
                    "context": request.context.to_bson()?
                },
                doc! {
                    "$set": {
                        "is_fully_verified": current.is_fully_verified.to_bson()?,
                        "fields": current.fields.to_bson()?
                    }
                },
                None,
            )
            .await?;
            // TODO: Check modified count.
        } else {
            coll.insert_one(request.to_document()?, None).await?;
        }

        Ok(())
    }
    fn gen_id(&mut self) -> i64 {
        self.event_counter += 1;
        self.event_counter
    }
    pub async fn process_message(&mut self, message: &ExternalMessage) -> Result<()> {
        let events = self.verify_message(message).await?;

        // Create event statement.
        let coll = self.db.collection(EVENT_COLLECTION);
        for event in events {
            coll.insert_one(Event::new(event, self.gen_id()).to_document()?, None)
                .await?;
        }

        Ok(())
    }
    async fn verify_message(&self, message: &ExternalMessage) -> Result<Vec<NotificationMessage>> {
        let coll = self.db.collection(IDENTITY_COLLECTION);

        // Fetch the current field state based on the message origin.
        let mut cursor = coll
            .find(
                doc! {
                    "fields.value": message.origin.to_bson()?,
                },
                None,
            )
            .await?;

        let mut events = vec![];

        // If a field was found, update it.
        while let Some(doc) = cursor.next().await {
            let mut id_state: JudgementState = from_document(doc?)?;
            let mut field_state = id_state
                .fields
                .iter_mut()
                .find(|field| field.value.matches(&message))
                // Technically, this should never return an error...
                .ok_or(anyhow!("Failed to select field when verifying message"))?;

            // If the message contains the challenge, set it as valid (or
            // invalid if otherwise).
            if !field_state.expected_challenge.is_verified {
                if field_state.expected_challenge.verify_message(&message) {
                    // Update field state. Be more specific with the query in order
                    // to verify the correct field (in theory, there could be
                    // multiple pending requests with the same external account
                    // specified).
                    coll.update_one(
                        doc! {
                            "fields.value": message.origin.to_bson()?,
                            "fields.expected_challenge.value": field_state.expected_challenge.value.to_bson()?,
                        },
                        doc! {
                            "$set": {
                                "fields.$.expected_challenge.is_verified": true.to_bson()?,
                            }
                        },
                        None,
                    )
                    .await?;

                    events.push(NotificationMessage::FieldVerified(
                        id_state.context.clone(),
                        field_state.value.clone(),
                    ));
                } else {
                    // Update field state.
                    coll.update_one(
                        doc! {
                            "fields.value": message.origin.to_bson()?,
                        },
                        doc! {
                            "$inc": {
                                "fields.$.failed_attempts": 1isize.to_bson()?,
                            }
                        },
                        None,
                    )
                    .await?;

                    events.push(NotificationMessage::FieldVerificationFailed(
                        id_state.context.clone(),
                        field_state.value.clone(),
                    ));
                }
            } else if let Some(challenge) = &field_state.second_expected_challenge {
            } else {
                continue;
            }

            // Check if all fields have been verified.
            std::mem::drop(field_state);

            if id_state.is_fully_verified() {
                coll.update_one(
                    doc! {
                        "context": id_state.context.to_bson()?,
                    },
                    doc! {
                        "$set": {
                            "is_fully_verified": true.to_bson()?
                        }
                    },
                    None,
                )
                .await?;

                events.push(NotificationMessage::IdentityFullyVerified(
                    id_state.context.clone(),
                ));
            }
        }

        Ok(events)
    }
    pub async fn fetch_events(&mut self) -> Result<Vec<NotificationMessage>> {
        let coll = self.db.collection(EVENT_COLLECTION);

        let mut cursor = coll
            .find(
                doc! {
                    "id": {
                        "$gt": self.event_counter.to_bson()?,
                    }
                },
                None,
            )
            .await?;

        let mut events = vec![];
        while let Some(doc) = cursor.next().await {
            let event = from_document::<Event>(doc?)?;

            // Track latest Id.
            self.event_counter = self.event_counter.max(event.id);
            events.push(event.event);
        }

        Ok(events)
    }
    pub async fn fetch_judgement_state(
        &self,
        context: &IdentityContext,
    ) -> Result<Option<JudgementState>> {
        let coll = self.db.collection(IDENTITY_COLLECTION);

        // Find the context.
        let doc = coll
            .find_one(
                doc! {
                    "context": context.to_bson()?,
                },
                None,
            )
            .await?;

        if let Some(doc) = doc {
            Ok(Some(from_document(doc)?))
        } else {
            // Not active request exists.
            Ok(None)
        }
    }
    pub async fn log_judgement_provided(&mut self, context: IdentityContext) -> Result<()> {
        let coll = self.db.collection(EVENT_COLLECTION);

        coll.insert_one(
            Event::new(
                NotificationMessage::JudgementProvided(context),
                self.gen_id(),
            )
            .to_document()?,
            None,
        )
        .await?;

        Ok(())
    }
}