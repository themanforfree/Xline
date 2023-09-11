use std::sync::Arc;

use curp_external_api::{
    cmd::{Command, ProposeId},
    LogIndex,
};
use serde::{Deserialize, Serialize};

use crate::rpc::ConfChangeEntry;

/// Log entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct LogEntry<C> {
    /// Term
    pub(crate) term: u64,
    /// Index
    pub(crate) index: LogIndex,
    /// Entry data
    pub(crate) entry_data: EntryData<C>,
}

/// Entry data of a `LogEntry`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum EntryData<C> {
    /// Empty entry
    Empty,
    /// `Command` entry
    Command(Arc<C>),
    /// `ConfChange` entry
    ConfChange(Box<ConfChangeEntry>),
    /// `Shutdown` entry
    Shutdown,
}

impl<C> LogEntry<C>
where
    C: Command,
{
    /// Create a new `LogEntry` of `Command`
    pub(super) fn new_empty(index: LogIndex, term: u64) -> Self {
        Self {
            term,
            index,
            entry_data: EntryData::Empty,
        }
    }

    /// Create a new `LogEntry` of `Command`
    pub(super) fn new_cmd(index: LogIndex, term: u64, cmd: Arc<C>) -> Self {
        Self {
            term,
            index,
            entry_data: EntryData::Command(cmd),
        }
    }

    /// Create a new `LogEntry` of `Shutdown`
    pub(super) fn new_shutdown(index: LogIndex, term: u64) -> Self {
        Self {
            term,
            index,
            entry_data: EntryData::Shutdown,
        }
    }

    /// Create a new `LogEntry` of `ConfChange`
    pub(super) fn new_conf_change(
        index: LogIndex,
        term: u64,
        conf_change: ConfChangeEntry,
    ) -> Self {
        Self {
            term,
            index,
            entry_data: EntryData::ConfChange(Box::new(conf_change)),
        }
    }

    /// Get the id of the entry
    pub(super) fn id(&self) -> &ProposeId {
        match self.entry_data {
            EntryData::Command(ref cmd) => cmd.id(),
            EntryData::ConfChange(ref e) => e.id(),
            EntryData::Shutdown | EntryData::Empty => {
                unreachable!("LogEntry::id() should not be called on a shutdown entry")
            }
        }
    }
}
