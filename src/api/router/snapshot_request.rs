//! Bounded raw JSON input for the MST/2 POST surface (spec 14).

use std::{collections::HashSet, fmt, time::Duration};

use axum::{
    extract::{FromRequest, Request},
    http::StatusCode,
};
use bytes::Bytes;
use serde::{
    Deserialize, Deserializer,
    de::{self, MapAccess, SeqAccess, Visitor},
};

use crate::ceres::snapshot::error::{SnapshotError, SnapshotErrorCode};

/// One overall read deadline, including a body which keeps trickling bytes.
/// This bounds input collection only, not handler work or response streams.
pub(super) const JSON_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Preserve the original bytes for TreeFrame request-body digests. The router
/// supplies DefaultBodyLimit; its rejection is converted to the MST envelope.
pub(super) struct Mst2Bytes(pub(super) Bytes);

/// Decode keys before comparing them, including escaped spellings. This
/// separate pass also catches duplicate optional fields whose first value is
/// null, which a derived DTO can otherwise treat as an absent field.
pub(super) fn validate_json_keys(body: &[u8]) -> Result<(), serde_json::Error> {
    serde_json::from_slice::<UniqueKeys>(body).map(|_| ())
}

struct UniqueKeys;

impl<'de> Deserialize<'de> for UniqueKeys {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(UniqueKeyVisitor)
    }
}

struct UniqueKeyVisitor;

impl<'de> Visitor<'de> for UniqueKeyVisitor {
    type Value = UniqueKeys;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("JSON without duplicate object keys")
    }

    fn visit_bool<E: de::Error>(self, _: bool) -> Result<UniqueKeys, E> {
        Ok(UniqueKeys)
    }
    fn visit_i64<E: de::Error>(self, _: i64) -> Result<UniqueKeys, E> {
        Ok(UniqueKeys)
    }
    fn visit_u64<E: de::Error>(self, _: u64) -> Result<UniqueKeys, E> {
        Ok(UniqueKeys)
    }
    fn visit_f64<E: de::Error>(self, _: f64) -> Result<UniqueKeys, E> {
        Ok(UniqueKeys)
    }
    fn visit_str<E: de::Error>(self, _: &str) -> Result<UniqueKeys, E> {
        Ok(UniqueKeys)
    }
    fn visit_unit<E: de::Error>(self) -> Result<UniqueKeys, E> {
        Ok(UniqueKeys)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<UniqueKeys, A::Error> {
        while sequence.next_element::<UniqueKeys>()?.is_some() {}
        Ok(UniqueKeys)
    }

    fn visit_map<A: MapAccess<'de>>(self, mut object: A) -> Result<UniqueKeys, A::Error> {
        let mut keys = HashSet::new();
        while let Some(key) = object.next_key::<String>()? {
            if !keys.insert(key) {
                return Err(de::Error::custom("duplicate JSON object key"));
            }
            object.next_value::<UniqueKeys>()?;
        }
        Ok(UniqueKeys)
    }
}

impl<S> FromRequest<S> for Mst2Bytes
where
    S: Send + Sync,
{
    type Rejection = SnapshotError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match tokio::time::timeout(JSON_REQUEST_TIMEOUT, Bytes::from_request(req, state)).await {
            Ok(Ok(body)) => Ok(Self(body)),
            Ok(Err(error)) => {
                let (code, message) = if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
                    (
                        SnapshotErrorCode::LimitExceeded,
                        "request body over the spec 14 limit",
                    )
                } else {
                    (
                        SnapshotErrorCode::InvalidRequest,
                        "could not read request body",
                    )
                };
                Err(SnapshotError::new(code, message))
            }
            Err(_) => Err(SnapshotError::new(
                SnapshotErrorCode::TemporaryUnavailable,
                "request body read deadline exceeded",
            )),
        }
    }
}
