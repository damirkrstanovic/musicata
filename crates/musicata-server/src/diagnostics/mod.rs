// SPDX-License-Identifier: AGPL-3.0-or-later
mod export;
pub mod http;
pub mod profiles;
mod recorder;
pub mod runtime;
pub use recorder::*;

#[derive(Clone)]
pub struct IncidentRecorded;

#[derive(Clone)]
pub struct IncidentCause(pub String);
