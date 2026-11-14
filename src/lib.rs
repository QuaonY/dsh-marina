//! Core data and storage for DSH Marina.

pub mod applog;
pub mod bisect;
pub mod catalog;
pub mod copy;
pub mod crash;
pub mod download;
pub mod dsh_cli;
pub mod external;
pub mod health;
pub mod history;
pub mod i18n;
pub mod instances;

pub mod launch;
pub mod links;
pub mod logs;
pub mod market;
pub mod net;
pub mod packs;
pub mod plugins;
pub mod relocate;

#[cfg(windows)]
pub mod platform;

#[cfg(windows)]
pub mod process;

pub mod runtime;
pub mod runtimes;
pub mod sensitive;
pub mod settings;
pub mod snapshots;
pub mod storage;
