//! Load doors: the contract between the streaming side (which spawns doors and performs the
//! crossing) and the player side (which lets a person open them).
//!
//! Streaming attaches [`LoadDoor`] to every spawned reference that has a `door_links` row, and
//! handles [`ActivateDoor`]: it makes the destination the active cell and moves the
//! [`StreamingCamera`](crate::world::components::StreamingCamera) to the arrival point. The player
//! controller only reads `LoadDoor` (to find the door in front of the player) and writes
//! `ActivateDoor`. Neither side depends on the other's internals.

use bevy::prelude::*;

/// How far from a door its return link's arrival point has to be for the point itself to say which
/// way the door faces, in Creation units. Closer than this the point is inside the doorway - many
/// links put the arriving player on the door's own position - and only the link's heading says
/// anything.
pub const OUTWARD_POINT_DISTANCE: f32 = 16.0;

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
    /// The horizontal unit direction the door faces, in Creation-engine axes - the side a player
    /// walks in from - or `None` when nothing in the database says which side that is.
    ///
    /// Taken from the link that leads *back* to this door ([`outward_from_return_link`]): the
    /// `XTEL` of a door that opens into this one puts its arrival point in front of this door, and
    /// its arrival heading faces away from it. That is world-space evidence, independent of the
    /// door's model. Door models do not agree on which of their own axes is their front: measured
    /// against where the game puts the arriving player, 43% of Skyrim.esm's load door placements
    /// are 180 degrees from their model's local `+Y` heading and 22% are at 270, so a model axis
    /// is a poor substitute when this is `None` (`crate::portal`, `crate::player`).
    pub outward: Option<[f32; 3]>,
}

/// The outward direction a door's return link gives it, in Creation-engine axes - or `None` when
/// that link carries no usable direction.
///
/// A link that leads *to* a door records where the player stands after coming through it, and the
/// game puts that arrival point in front of the door, facing away from it. So the door faces from
/// its own position toward `arrival_position`; when the point is nearer than
/// [`OUTWARD_POINT_DISTANCE`] it is inside the doorway and cannot say which way the door faces, and
/// the arrival heading `(sin z, cos z, 0)` - a Creation heading measured clockwise from north, the
/// way the arriving player looks - is used instead. `arrival_rotation` is the whole `XTEL` arrival
/// rotation; only its `z` is read.
pub fn outward_from_return_link(
    door_position: [f32; 3],
    arrival_position: [f32; 3],
    arrival_rotation: [f32; 3],
) -> Option<[f32; 3]> {
    let offset = Vec3::new(
        arrival_position[0] - door_position[0],
        arrival_position[1] - door_position[1],
        0.0,
    );
    if offset.is_finite() && offset.length() > OUTWARD_POINT_DISTANCE {
        let direction = offset.normalize();
        return Some([direction.x, direction.y, 0.0]);
    }
    let heading = Vec3::new(arrival_rotation[2].sin(), arrival_rotation[2].cos(), 0.0);
    (heading.is_finite() && heading.length_squared() > 0.5).then_some([heading.x, heading.y, 0.0])
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Skyrim.esm's ruined tower door `0005BDF1`, whose return link gives its facing: the door at
    /// 73550, 78431 and the arrival frame of the link that leads to it - 32 units east (782 in
    /// Creation `x`), heading 92 degrees.
    const TOWER_DOOR: [f32; 3] = [73550.0, 78431.0, -5609.0];
    const TOWER_ARRIVAL: [f32; 3] = [73582.0, 78430.0, -5609.0];
    const TOWER_HEADING: [f32; 3] = [0.0, 0.0, 92.0_f32.to_radians()];

    #[test]
    fn the_arrival_point_of_the_return_link_is_the_way_the_door_faces() {
        let outward = outward_from_return_link(TOWER_DOOR, TOWER_ARRIVAL, TOWER_HEADING).unwrap();
        assert!(
            outward[0] > 0.99 && outward[1].abs() < 0.05 && outward[2] == 0.0,
            "32 units east of the door is east: {outward:?}"
        );
        assert!(
            (outward[0].hypot(outward[1]) - 1.0).abs() < 1.0e-5,
            "the direction is a unit vector: {outward:?}"
        );

        // The point is evidence in its own right: a contradictory heading does not overrule it when
        // it is clearly outside the doorway.
        let outward = outward_from_return_link(
            TOWER_DOOR,
            TOWER_ARRIVAL,
            [0.0, 0.0, 270.0_f32.to_radians()],
        )
        .unwrap();
        assert!(outward[0] > 0.99, "the point wins at 32 units: {outward:?}");
    }

    #[test]
    fn an_arrival_point_inside_the_doorway_leaves_the_heading_to_say_which_way_it_faces() {
        // The point lands on the door itself: only the heading says anything, even a heading that
        // points the other way from where the point happens to be.
        let beneath = [TOWER_DOOR[0] - 10.0, TOWER_DOOR[1] + 8.0, TOWER_DOOR[2]];
        let outward = outward_from_return_link(TOWER_DOOR, beneath, TOWER_HEADING).unwrap();
        assert!(
            outward[0] > 0.99 && outward[1].abs() < 0.05,
            "the heading is east: {outward:?}"
        );

        // The same heading, the arrival point exactly on the door: still east, and still a unit
        // vector, not a zero one.
        for offset in [[0.0, 0.0, 0.0], [15.0, 0.0, 0.0], [0.0, -15.0, 0.0]] {
            let position = [
                TOWER_DOOR[0] + offset[0],
                TOWER_DOOR[1] + offset[1],
                TOWER_DOOR[2] + offset[2],
            ];
            let outward = outward_from_return_link(TOWER_DOOR, position, TOWER_HEADING).unwrap();
            assert!(
                (outward[0].hypot(outward[1]) - 1.0).abs() < 1.0e-5,
                "{offset:?}: {outward:?}"
            );
            assert!(outward[0] > 0.99, "{offset:?}: {outward:?}");
        }

        // A heading of 0 is north, `(0, 1, 0)` in Creation axes.
        let north = outward_from_return_link(TOWER_DOOR, TOWER_DOOR, [0.0, 0.0, 0.0]).unwrap();
        assert!(
            north[0].abs() < 1.0e-6 && (north[1] - 1.0).abs() < 1.0e-6,
            "heading 0 is Creation +Y: {north:?}"
        );
    }

    /// Nothing to go on: a link whose arrival frame is not a direction at all. The caller has no
    /// outward direction for the door and falls back to the door model's own axes.
    #[test]
    fn an_unusable_arrival_frame_gives_no_direction() {
        // A broken arrival point is ignored while the heading still says something.
        let heading_only =
            outward_from_return_link(TOWER_DOOR, [f32::NAN, 0.0, 0.0], TOWER_HEADING).unwrap();
        assert!(heading_only[0] > 0.99, "{heading_only:?}");

        assert_eq!(
            outward_from_return_link(TOWER_DOOR, TOWER_DOOR, [0.0, 0.0, f32::NAN]),
            None,
            "a point in the doorway and no heading is no direction"
        );
        assert_eq!(
            outward_from_return_link(
                [f32::NAN; 3],
                [TOWER_DOOR[0], TOWER_DOOR[1], 0.0],
                [0.0, 0.0, f32::NAN]
            ),
            None
        );
    }
}
