//! Content, users, hooks, media, request routing, the scheduler, configuration, the
//! definitions of custom post types, taxonomies and field groups, plugin manifests, and the
//! admin's authorization rules: the core of Mission Control.

pub mod admin;
pub mod config;
pub mod content;
pub mod front;
pub mod hooks;
pub mod media;
pub mod migrations;
pub mod plugins;
pub mod rest;
pub mod scheduler;
mod text;
pub mod theme;
pub mod types;
pub mod users;

pub mod install;
