//! A guest implementation of the `root` world from `wit/world.wit`.
//!
//! It imports the host's `score` interface (`finish: func(x: u64)`) and exports
//! the `score-progress` interface, whose `utxo` resource owns a mutable
//! `storage` record.

mod bindings {
    use super::Component;

    wit_bindgen::generate!({
        world: "root",
        path: "wit",
    });

    export!(Component);
}

use core::cell::RefCell;

use bindings::exports::root::component::score_progress::{
    Guest, GuestUtxo, Storage, Utxo, UtxoBorrow,
};

struct Component;

impl Guest for Component {
    type Utxo = ScoreUtxo;

    fn get_storage(utxo: UtxoBorrow) -> Storage {
        utxo.get::<Self::Utxo>().storage.borrow().clone()
    }

    fn set_storage(utxo: UtxoBorrow, storage: Storage) {
        *utxo.get::<Self::Utxo>().storage.borrow_mut() = storage;
    }
}

/// Backing state for an exported `utxo` resource handle.
struct ScoreUtxo {
    storage: RefCell<Storage>,
}

impl GuestUtxo for ScoreUtxo {
    /// `main fn new()` in `example.star`. The Starstream source yields the
    /// `Score` ABI and, once resumed, emits `Finish(chips * mult)`. The host
    /// doesn't drive yield/resume coroutines yet, so we flatten it: `new` just
    /// mints the handle with zeroed storage, the ABI methods mutate it, and the
    /// `Finish` event fires from [`finish`](Self::finish) (the resume point).
    fn new() -> Utxo {
        Utxo::new(ScoreUtxo {
            storage: RefCell::new(Storage { chips: 0, mult: 0 }),
        })
    }

    /// `fn plus_chips(chips2) { chips = chips + chips2; }`
    fn plus_chips(&self, chips2: u64) {
        self.storage.borrow_mut().chips += chips2;
    }

    /// `fn plus_mult(mult2) { mult = mult + mult2; }`
    fn plus_mult(&self, mult2: u64) {
        self.storage.borrow_mut().mult += mult2;
    }

    /// `fn mult_mult(mult_pct) { mult = mult * mult_pct / 100; }`
    fn mult_mult(&self, mult_pct: u64) {
        let mut storage = self.storage.borrow_mut();
        storage.mult = storage.mult * mult_pct / 100;
    }

    /// `fn finish() { resume; }` — resuming `new` emits `Finish(chips * mult)`,
    /// which lowers to the imported `score.finish(x: u64)` host function.
    fn finish(&self) {
        let storage = self.storage.borrow();
        bindings::root::component::score::finish(storage.chips * storage.mult);
    }
}
