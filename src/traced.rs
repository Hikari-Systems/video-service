//! Client spans for the calls that are not reqwest: the AWS SDK's S3 and
//! MediaConvert clients. Reqwest calls use hs-utils' `.send_traced()`.
//!
//! ```rust,ignore
//! use crate::traced::Traced as _;
//! client.head_object().bucket(b).key(k).send().traced(crate::traced::s3("HeadObject", b)).await
//! ```
//!
//! The span names the operation and the bucket, never the key (keys carry
//! video ids) or anything in the body.

use std::future::Future;

use tracing::{Instrument, Span};

/// `.traced(span)` on a fallible future: runs it inside `span` and records
/// `client.outcome` (`ok` / `error`), which `hs.client.operation.duration` is
/// labelled by.
pub trait Traced<T, E>: Future<Output = Result<T, E>> + Sized {
    fn traced(self, span: Span) -> impl Future<Output = Result<T, E>> + Send
    where
        Self: Send,
        T: Send,
        E: Send,
    {
        async move {
            let out = self.instrument(span.clone()).await;
            hs_utils::telemetry::record_outcome(&span, if out.is_ok() { "ok" } else { "error" });
            out
        }
    }
}

impl<F, T, E> Traced<T, E> for F where F: Future<Output = Result<T, E>> {}

/// An S3 call: `s3 <Operation>`, against `bucket`.
pub fn s3(operation: &'static str, bucket: &str) -> Span {
    hs_utils::telemetry::client_span("s3", operation, bucket)
}

/// A MediaConvert call: `mediaconvert <Operation>`.
pub fn mediaconvert(operation: &'static str) -> Span {
    hs_utils::telemetry::client_span("mediaconvert", operation, "mediaconvert")
}
