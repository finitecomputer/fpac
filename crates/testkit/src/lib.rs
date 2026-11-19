//! `fpac-testkit`: the folder remote (a stand-in for the platform's route
//! to the object store, named as one in every result) and the conformance
//! suite (docs/contract.md, Conformance), which runs against this remote
//! in tests and against the deployed stand-in Worker from inside a guest.

pub mod conformance;
pub mod folder;
