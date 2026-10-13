//! **Stores as machines**: the data the player reads (the playing item) physically owned by one
//! [`Stores`] aggregate. A [`StoreCmd`] is the complete, enumerated vocabulary of mutations, each
//! store's own command decoder is the one place it is applied, and every command that changes the
//! store raises the store's NOTICE (a generation the dispatcher delivers to every live instance
//! as `ScreenEvent::StoreChanged`).

use plx_machine::machine::StoreOrd;

pub mod metadata;

/// The production store aggregate.
#[derive(Default)]
pub struct Stores {
    pub metadata: metadata::MetadataStore,
}

impl Stores {
    pub fn metadata_run(&mut self, cmd: metadata::MetadataCmd) -> bool {
        self.metadata.run(cmd)
    }

    pub fn metadata_view(&self) -> crate::metadata::MetadataView<'_> {
        self.metadata.view()
    }

    pub fn gen(&self, id: StoreId) -> u32 {
        match id {
            StoreId::Metadata => self.metadata.gen(),
        }
    }

    pub fn take_notices(&self) -> Vec<(StoreId, u32)> {
        self.metadata.take_notice().map(|generation| (StoreId::Metadata, generation)).into_iter().collect()
    }
}

/// The application's stores, in the library's ordinal order (`StoreOrd`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StoreId {
    Metadata,
}

impl StoreId {
    pub const ALL: [StoreId; 1] = [StoreId::Metadata];

    /// The library's ordinal for this store (the library never names `StoreId`).
    pub fn ord(self) -> StoreOrd {
        StoreOrd(self as u32)
    }

    pub fn from_ord(o: StoreOrd) -> Option<StoreId> {
        StoreId::ALL.get(o.0 as usize).copied()
    }

    pub fn name(self) -> &'static str {
        match self {
            StoreId::Metadata => "metadata",
        }
    }
}

/// Every mutation of every store, nested per store.
#[derive(Clone)]
pub enum StoreCmd {
    Metadata(metadata::MetadataCmd),
}

impl StoreCmd {
    pub fn store(&self) -> StoreId {
        match self {
            StoreCmd::Metadata(_) => StoreId::Metadata,
        }
    }
}
