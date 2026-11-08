// SPDX-License-Identifier: GPL-3.0-or-later  (PROVISIONAL: the owner has not decided the licence yet)
//! DexHid core: everything that can be tested without devices, a compositor or a bus.
//! Written from scratch for DexHid; behaviour numbers cited from Denial's INPUT_DECK.md, no Denial code used.

pub mod settings;
pub mod modes;
// M3
pub mod extras;
pub mod gadget;
pub mod gestures;
pub mod hid;
pub mod pen;
pub mod profiles;
pub mod quick;
pub mod mouse;
pub mod feedback;
pub mod arrangement;
pub mod chord;
pub mod config;
pub mod geom;
pub mod hotplug;
pub mod keycodes;
pub mod keys;
pub mod layout;
pub mod mt;
pub mod router;
pub mod swipe;
pub mod state;
pub mod touchpad;
