//! The playing item, as a store over `crate::metadata`.
//!
//! Reads go through a [`MetadataView`](crate::metadata::MetadataView) (`store.view()`), which a
//! screen may call from `step` or `draw`. Writes are the [`MetadataCmd`] variants, applied by
//! [`MetadataStore::run`], each of which raises the store's notice.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use plx_machine::machine::{Cx, Effects, Handled, Host, Machine};

#[derive(Clone)]
pub enum MetadataCmd {
    /// A playback starts with this item (`None` retires the previous one without a successor).
    InstallPlaying(Option<crate::metadata::PlayingItem>),
    /// A playback is starting: no panel may read the previous item until its own is installed.
    RetirePlayingItem,
}

/// One Metadata owner: the state and its notice.
#[derive(Default)]
pub struct MetadataStore {
    state: crate::metadata::MetadataState,
    notice_gen: AtomicU32,
    notice_dirty: AtomicBool,
}

impl MetadataStore {
    fn bump(&self) -> u32 {
        self.notice_dirty.store(true, Ordering::Relaxed);
        self.notice_gen.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub fn gen(&self) -> u32 {
        self.notice_gen.load(Ordering::Relaxed)
    }

    pub fn take_notice(&self) -> Option<u32> {
        self.notice_dirty.swap(false, Ordering::Relaxed).then(|| self.gen())
    }

    pub fn state(&self) -> &crate::metadata::MetadataState {
        &self.state
    }

    /// Apply one command and raise the notice.
    pub fn run(&mut self, cmd: MetadataCmd) -> bool {
        let changed = crate::metadata::run(&mut self.state, cmd);
        self.bump();
        changed
    }

    /// Borrowed read handle.
    pub fn view(&self) -> crate::metadata::MetadataView<'_> {
        crate::metadata::MetadataView::new(self)
    }
}

impl<H: Host> Machine<H> for MetadataStore {
    type Ev = MetadataCmd;
    fn step(&mut self, ev: &Self::Ev, _cx: &Cx<'_, H>, _fx: &mut Effects<'_, H>) -> Handled {
        self.run(ev.clone());
        Handled::Yes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_raises_the_notice_once() {
        let mut store = MetadataStore::default();
        assert_eq!(store.take_notice(), None);
        store.run(MetadataCmd::InstallPlaying(Some(crate::metadata::PlayingItem::with_subs(Vec::new()))));
        assert_eq!(store.take_notice(), Some(1));
        assert_eq!(store.take_notice(), None, "taken");
        assert!(store.view().playing().is_some());
        store.run(MetadataCmd::RetirePlayingItem);
        assert!(store.view().playing().is_none());
    }
}
