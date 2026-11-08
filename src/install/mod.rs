pub(crate) mod check;
pub mod command;
pub(crate) mod commit;
pub mod destination;
pub mod inventory;
pub(crate) mod journal;
pub mod plan;
pub(crate) mod receipt;
pub mod receipt_store;
pub(crate) mod removal;
pub(crate) mod scope;
pub mod update;

#[cfg(all(test, windows))]
pub(crate) mod windows_test_support;
