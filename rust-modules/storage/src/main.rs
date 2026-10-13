#[path = "../../platform/src/storage_service/backend.rs"]
#[allow(dead_code)]
mod backend;
#[path = "../../platform/src/storage_service/keymanager.rs"]
#[allow(dead_code)]
mod keymanager;
#[path = "../../platform/src/storage_service/runtime.rs"]
#[allow(dead_code)]
mod runtime;
#[path = "../../platform/src/storage/state.rs"]
#[allow(dead_code)] // Shared engine also exposes adapter APIs unused by this executable.
mod state;
#[path = "../../platform/src/storage_service/wire.rs"]
#[allow(dead_code)]
mod wire;


fn main() {
    {
        eprintln!("plxnative-storage requires the webOS Linux service runtime");
        std::process::exit(1);
    }
}
