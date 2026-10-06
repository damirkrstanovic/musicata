// SPDX-License-Identifier: AGPL-3.0-or-later
mod export;
pub mod http;
mod recorder;
pub use recorder::*;

#[derive(Clone)]
pub struct IncidentRecorded;

#[derive(Clone)]
pub struct IncidentCause(pub String);
