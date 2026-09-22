//! Load doors: the contract between the streaming side (which spawns doors and performs the
//! crossing) and the player side (which lets a person open them).
//!
//! Streaming attaches [`LoadDoor`] to every spawned reference that has a `door_links` row, and
//! handles [`ActivateDoor`]: it makes the destination the active cell and moves the
//! [`StreamingCamera`](crate::world::components::StreamingCamera) to the arrival point. The player
//! controller only reads `LoadDoor` (to find the door in front of the player) and writes
//! `ActivateDoor`. Neither side depends on the other's internals.

use bevy::prelude::*;

/// Where a load door leads, as converted from the source door's `XTEL` subrecord.
#[derive(Debug, Clone, PartialEq)]
pub struct DoorDestination {
    /// The destination door reference (`XTEL` bytes 0..4, after plugin FormID remapping).
    pub destination_ref_id: u32,
    /// `Some(cell_id)` when the destination is an interior cell (`cells.worldspace_id IS NULL`).
    pub interior_cell_id: Option<u32>,
    /// `Some(worldspace_id)` when the destination is an exterior (or a persistent cell) of a
    /// worldspace. Exactly one of `interior_cell_id` / `worldspace_id` is `Some` for a usable door.
    pub worldspace_id: Option<u32>,
    /// Arrival position in Creation-engine units (`XTEL` bytes 4..16). This is **not** the
    /// destination door's own position.
    pub arrival_position: [f32; 3],
    /// Arrival rotation in Creation-engine radians (`XTEL` bytes 16..28).
    pub arrival_rotation: [f32; 3],
}

/// Marks a spawned reference as a load door. Lives on the reference's root entity, whose
/// `GlobalTransform` is the door's placement in the render world.
#[derive(Component, Debug, Clone, PartialEq)]
pub struct LoadDoor {
    /// The door reference's own FormID.
    pub ref_id: u32,
    pub destination: DoorDestination,
    /// Human-readable destination for the interaction prompt, e.g. "Alftand Glacial Ruins" or
    /// "Blackreach": the destination interior cell's name, else the worldspace's editor id.
    pub label: String,
    /// True for Skyrim's auto-load doors: invisible markers (`AutoLoadDoor01` and friends, model
    /// `AutoLoadMarker01.nif`) that cross the moment the player walks into them. The player
    /// controller crosses these on contact and offers no `E` prompt; every other load door keeps
    /// the `E` key.
    pub auto_load: bool,
}

/// Request to go through a load door. Written by the player controller (E key) or by a test;
/// read by the streaming side, which ignores entities that are gone or carry no [`LoadDoor`].
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActivateDoor {
    pub door: Entity,
}

/// Sent by the streaming side after a crossing completes, so the player controller can reset
/// its velocity and orientation and the HUD can show where the player arrived.
#[derive(Message, Debug, Clone, PartialEq)]
pub struct DoorCrossed {
    pub from_ref_id: u32,
    pub label: String,
}
