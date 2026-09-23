//! Load doors: the contract between the streaming side (which spawns doors and performs the
//! crossing) and the player side (which lets a person open them).
//!
//! Streaming attaches [`LoadDoor`] to every spawned reference that has a `door_links` row, and
//! keeps the door's [`DoorState`] - which is what everything that has to agree on "is this door
//! open" reads. The player controller reads `LoadDoor` (to find the door in front of the player)
//! and its state, and writes [`OpenDoor`](crate::transition::OpenDoor): the door swings, and the
//! player walks through the doorway, where the crossing fires ([`crate::transition`]).
//! [`ActivateDoor`] is the scripted run's crossing - a snap with no walking - and neither side
//! depends on the other's internals.

use bevy::prelude::*;

/// How far from a door its return link's arrival point has to be for the point itself to say which
/// way the door faces, in Creation units. Closer than this the point is inside the doorway - many
/// links put the arriving player on the door's own position - and only the link's heading says
/// anything.
pub const OUTWARD_POINT_DISTANCE: f32 = 16.0;

/// How far from a door its return link's arrival point may be and still be trusted as a point, in
/// Creation units.
///
/// A link that leads back into a door puts the arriving player in front of it, so the point it
/// lands on says which way the door faces - but only while it is *at* the door. A link that arrives
/// hundreds of units away is pointing into the room the door is the way into rather than at the
/// door, and the direction from the door to the far end of a room is not the direction the door
/// faces. Past this the heading is used instead, which is the direction the arriving player faces
/// and so the way the door does.
pub const OUTWARD_POINT_MAX_DISTANCE: f32 = 512.0;

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
/// its own position toward `arrival_position` - but only while that point is at the door: nearer
/// than [`OUTWARD_POINT_DISTANCE`] it is inside the doorway, and farther than
/// [`OUTWARD_POINT_MAX_DISTANCE`] it is somewhere else in the room, and neither says which way the
/// door faces. In both cases the arrival heading `(sin z, cos z, 0)` - a Creation heading measured
/// clockwise from north, the way the arriving player looks - is used instead.
/// `arrival_rotation` is the whole `XTEL` arrival rotation; only its `z` is read.
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
    let distance = offset.length();
    if offset.is_finite()
        && distance > OUTWARD_POINT_DISTANCE
        && distance <= OUTWARD_POINT_MAX_DISTANCE
    {
        let direction = offset.normalize();
        return Some([direction.x, direction.y, 0.0]);
    }
    let heading = Vec3::new(arrival_rotation[2].sin(), arrival_rotation[2].cos(), 0.0);
    (heading.is_finite() && heading.length_squared() > 0.5).then_some([heading.x, heading.y, 0.0])
}

// ---------------------------------------------------------------------------------------------
// The doorway anchor
// ---------------------------------------------------------------------------------------------

/// How far the game's own arrival point may stand from the destination doorway's centre, in plan,
/// in Creation units, and still be read as "the game lands the player in this doorway".
///
/// About one and a half doorway widths: past this the `XTEL` is pointing at somewhere else in the
/// room rather than at the door, and the anchored map would move the player further from the
/// game's own landing than the doorway is wide
/// (`docs/research/portal-door-alignment.md` section 9.2).
pub const ANCHOR_PLAN_CAP: f32 = 256.0;

/// How far the anchored landing may stand above or below the destination floor, in units, before
/// the door keeps the arrival anchor.
///
/// The walk re-grounds a landing only within [`crate::player`]'s step-up and snap-down distances
/// (40 and 60 units): a landing more than [`crate::player::STEP_HEIGHT`] *below* the destination
/// floor would leave the player falling under it. So the cap is the step height, the smaller of
/// the two, and a landing inside it is absorbed by the walk either way. The report's tool gates at
/// 64 (`docs/research/portal-door-alignment.md` sections 4.3 and 9.2); the engine is stricter,
/// and the doors between the two keep the `XTEL` landing. The cap is also what keeps ladders,
/// trapdoors and other-level doors - whose whole point is the vertical - on that landing.
pub const ANCHOR_HEIGHT_CAP: f32 = crate::player::STEP_HEIGHT;

/// A load door reference's own doorway, as the converted database places it: the model's bounds
/// box centre under the reference's rotation and scale, the model it is, and the model's own axis
/// convention when the instal's placements agree on one.
///
/// Built by the database layer (`crate::world::database`, one row per reference with a
/// `door_links` row) and turned into a [`DoorAnchor`] by [`doorway_anchor`] in `crate::streaming`.
/// All lengths are Creation-engine units and all angles Creation-engine radians, as the database
/// stores them.
#[derive(Debug, Clone, PartialEq)]
pub struct DoorwayPlacement {
    /// The reference's own position (`references.pos_x..z`).
    pub position: [f32; 3],
    /// The reference's own rotation (`references.rot_x..z`), Creation-engine radians.
    pub rotation: [f32; 3],
    /// The reference's own `XSCL` scale, which scales the model bounds before the rotation.
    pub scale: f32,
    /// The **converted** model's bounds box centre in model space (runtime axes, Y up), when the
    /// base record has usable bounds. `None` for a base with no bounds at all - the invisible
    /// `AutoLoadMarker01` markers among them - and for a door whose base has no `statics` row.
    pub box_centre: Option<[f32; 3]>,
    /// The base record's model path. Two doors are "the same model" - tier 1 of the report's
    /// section 9.2, where no per-model convention is needed at all - when these are equal and
    /// non-empty.
    pub model: String,
    /// The model's own axis convention in radians, when the model's placements agree on one: the
    /// circular mean over every placement of the model of (link-derived facing - reference yaw).
    /// `None` for a model whose placements disagree (the invisible markers, ship trapdoors, ladder
    /// doors), which is what sends a door to tier 3.
    pub convention: Option<f32>,
    /// The reference's own cell grid, for a reference in an exterior cell. `None` for an interior,
    /// and for a cell the database cannot place on a grid.
    pub grid: Option<[i32; 2]>,
    /// The `z` of the arrival point of the link that leads **back** into this door: where the game
    /// stands the player who comes out of it. `None` for a door nothing leads back to.
    pub return_arrival_z: Option<f32>,
}

impl DoorwayPlacement {
    /// The Creation heading this doorway faces: the reference's own yaw plus its model's own axis
    /// convention, when that is known.
    pub fn facing(&self) -> Option<f32> {
        self.convention
            .map(|convention| self.rotation[2] + convention)
    }

    /// The doorway box centre placed by the reference, in runtime (Y up) coordinates: the
    /// reference's own origin without a box.
    fn placed_box_centre(&self) -> Vec3 {
        let origin = Vec3::from_array(shared::coordinates::creation_to_runtime_vector(
            self.position,
        ));
        origin + self.box_offset()
    }

    /// The box centre's offset from the reference origin, in runtime (Y up) units, placed by the
    /// reference's own rotation and scale. Zero without a box.
    fn box_offset(&self) -> Vec3 {
        let Some(centre) = self.box_centre else {
            return Vec3::ZERO;
        };
        let rotation = Quat::from_array(shared::coordinates::creation_euler_to_runtime_quaternion(
            self.rotation,
        ));
        rotation * (Vec3::from_array(centre) * self.scale)
    }

    /// The doorway box centre's height above the reference, in runtime (Y up) units: the box
    /// centre placed by the reference's own rotation and scale. Zero without a box.
    fn box_height(&self) -> f32 {
        self.box_offset().y
    }
}

/// Which of the four anchors of `docs/research/portal-door-alignment.md` section 9.2 a door was
/// given, decided from the data. A door the data does not support has no [`DoorAnchor`] at all -
/// the report's tier 4, today's `XTEL` arrival map, byte for byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DoorAnchorTier {
    /// The source and destination doors are the same base model, both have a doorway box and the
    /// arrival gate passes. The model's own axis convention cancels out of the map
    /// (`R(yawB + C) * Y180 * R(yawA + C)^-1 = R(yawB + 180 - yawA)`), so neither side needs one and
    /// the two references' own rotations are the whole of the facings.
    SameModel,
    /// Different models, both of whose placements agree on an axis convention: both doorway
    /// facings are known and the map is the doorway-to-doorway one.
    Conventions,
    /// A facing cannot be established for one of the two doorways: the doorway *centres* are
    /// anchored - which is the offset the user saw - and today's facings are kept, so the facing
    /// error stays.
    Centres,
}

/// A doorway as the map places it: the reference's own placement with the model's box centre, in
/// Creation-engine units. Every [`DoorAnchor`]'s doorways have one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DoorwayGeometry {
    pub position: [f32; 3],
    pub rotation: [f32; 3],
    pub scale: f32,
    /// The model's bounds box centre in model space, placed by the reference.
    pub box_centre: [f32; 3],
}

/// How the map's two frames are read from a [`DoorAnchor`]: which way each doorway faces, and
/// therefore which side a player walks in from. This is the *orientation* half of the anchor - the
/// centres are [`DoorwayGeometry`] - and it is not the same question as the map's turn, which is
/// only the doorways' facing *difference* and stays exact even where an absolute facing cannot be
/// had.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DoorwayFacings {
    /// Both doorways' own facings are known: the reference's yaw plus its model's own axis
    /// convention, in Creation headings. The map is built in the two doorways' own frames, so the
    /// room's turn, the side the player walks in from and the plane the window is clipped at are
    /// all the doorways' own.
    Known {
        /// The Creation heading the source doorway faces - the side a player walks in from.
        source: f32,
        /// The Creation heading the destination doorway faces.
        destination: f32,
    },
    /// One model on both sides whose own axis convention is *not* known, so neither doorway has an
    /// absolute facing. Their *difference* is still exact - the model's convention cancels out of
    /// it, which is the whole of the report's tier 1 - so the room is turned exactly as it should
    /// be; the side a player walks in from comes from the source door's link data instead
    /// (`crate::transition::door_frame`), which is where the engine has always read it.
    ///
    /// The price is the one number a model with no convention cannot give: the link-derived frame
    /// is off the doorway's true facing by however far that door's own link evidence is (median 3.0
    /// degrees over the install, p90 84.5), and the window's clip plane - which is built from this
    /// frame turned onto the destination - inherits it. The room's turn does not.
    SameModel {
        /// The Creation heading from the source doorway's facing to the destination's: the two
        /// references' own yaw difference, which is all that survives the cancellation.
        turn: f32,
    },
    /// No facing could be established at all (tier 3): today's are kept - the source door's
    /// link-derived frame, and the link's `XTEL` arrival heading for the destination.
    Kept,
}

/// The doorway anchor a door's map is built from: the source and destination doorways' own
/// geometry, in place of the link's `XTEL` arrival target.
///
/// Attached by `crate::streaming` when [`doorway_anchor`] accepts the door's data; read by
/// `crate::transition`'s map, which the portal camera, the crossing and the doorway mirror are all
/// placed by, and by the player's doorway plane, which has to stand in the same plane as the
/// portal's window. **Absent** on a door the data does not support - tier 4 of the report - and on
/// every door of a run without a world database, where the map is exactly what it has always been.
#[derive(Component, Debug, Clone, PartialEq)]
pub struct DoorAnchor {
    /// Which tier's rules produced this anchor.
    pub tier: DoorAnchorTier,
    /// The source doorway's box centre in the *model's* frame: the anchor's pivot is this point
    /// placed by the door's own reference
    /// (`position + scale * (rotation * source_box_centre)`).
    pub source_box_centre: [f32; 3],
    /// The destination door's own placement.
    pub destination: DoorwayGeometry,
    /// The destination reference's cell grid, for an exterior destination: the cell
    /// `door_links.destination_cell_id` resolved to, which is where the anchor lands the player and
    /// therefore what the streaming paths have to pre-stream. `None` for an interior destination.
    pub destination_grid: Option<[i32; 2]>,
    /// Which way the two doorways face, and whether that could be established.
    pub facings: DoorwayFacings,
}

/// The anchor a door whose link leads to `destination` is drawn, crossed and streamed with, or
/// `None` when the data does not support one and today's arrival anchor is the right answer.
///
/// The tiers and the arrival gate are `docs/research/portal-door-alignment.md` section 9.2, decided
/// per door from the data:
///
/// * no doorway box on either side, or an exterior destination with no resolved cell (nothing to
///   stream the right place for), or an arrival that does not land at the doorway: no anchor;
/// * the same base model on both sides: full doorway anchor, whose turn is exact with no convention
///   needed ([`DoorwayFacings::SameModel`] where the model's own axis is not known);
/// * both models' own axes known: full doorway anchor;
/// * anything else: the doorway *centres* anchored with today's facings.
///
/// `arrival_position` is the link's `XTEL` arrival point; `exterior_destination` is what the link
/// says the destination space is.
pub fn doorway_anchor(
    source: &DoorwayPlacement,
    destination: &DoorwayPlacement,
    arrival_position: [f32; 3],
    exterior_destination: bool,
) -> Option<DoorAnchor> {
    let (Some(source_box_centre), Some(destination_box_centre)) =
        (source.box_centre, destination.box_centre)
    else {
        return None;
    };
    if exterior_destination && destination.grid.is_none() {
        return None;
    }
    if !arrival_lands_at_the_doorway(source, destination, arrival_position) {
        return None;
    }
    let same_model = !source.model.is_empty() && source.model == destination.model;
    let both_facings_known = source.facing().zip(destination.facing());
    let (tier, facings) = match (same_model, both_facings_known) {
        (true, Some((source, destination))) => (
            DoorAnchorTier::SameModel,
            DoorwayFacings::Known {
                source,
                destination,
            },
        ),
        // One model on both sides with no convention between them: the map's *turn* is still exact
        // - the conventions cancel out of the doorways' facing difference, which is the two
        // references' own yaw difference and nothing else - while the side a player walks in from
        // has to come from the source door's link data instead.
        (true, None) => (
            DoorAnchorTier::SameModel,
            DoorwayFacings::SameModel {
                turn: destination.rotation[2] - source.rotation[2],
            },
        ),
        (false, Some((source, destination))) => (
            DoorAnchorTier::Conventions,
            DoorwayFacings::Known {
                source,
                destination,
            },
        ),
        (false, None) => (DoorAnchorTier::Centres, DoorwayFacings::Kept),
    };
    Some(DoorAnchor {
        tier,
        source_box_centre,
        destination: DoorwayGeometry {
            position: destination.position,
            rotation: destination.rotation,
            scale: destination.scale,
            box_centre: destination_box_centre,
        },
        destination_grid: destination.grid,
        facings,
    })
}

/// Whether the game's own arrival lands at the destination doorway: within [`ANCHOR_PLAN_CAP`] of
/// its centre in plan, and with the anchored landing within [`ANCHOR_HEIGHT_CAP`] of the
/// destination floor (`docs/research/portal-door-alignment.md` sections 4.3 and 9.2).
///
/// The height term is the two sides' floors measured against their own door's base - where the game
/// stands the player coming out of each door - plus the two doorway boxes' own heights above their
/// references, which is what the anchor adds to that difference. Its sign is the report's; the gate
/// takes the modulus, and the sign of the true error is the opposite one.
///
/// A door nothing leads back to has no floor level to measure against and no anchor.
fn arrival_lands_at_the_doorway(
    source: &DoorwayPlacement,
    destination: &DoorwayPlacement,
    arrival_position: [f32; 3],
) -> bool {
    let Some(return_arrival_z) = source.return_arrival_z else {
        return false;
    };
    // In plan, against the destination doorway's placed centre - not the reference origin, which
    // stands off it by however far the model's box is from its own origin - as the report's tool
    // measures it (`tools/research/portal_door_alignment.py`, `gap_lateral`/`gap_depth`).
    let arrival = Vec3::from_array(shared::coordinates::creation_to_runtime_vector(
        arrival_position,
    ));
    let doorway = destination.placed_box_centre();
    let plan = (arrival.x - doorway.x).hypot(arrival.z - doorway.z);
    let height_error = (arrival_position[2] - destination.position[2])
        - (return_arrival_z - source.position[2])
        + destination.box_height()
        - source.box_height();
    plan <= ANCHOR_PLAN_CAP && height_error.abs() <= ANCHOR_HEIGHT_CAP
}

/// How far through its `Open` clip a load door counts as open, as a fraction of the clip's length.
///
/// The clip keeps playing past this point to its end and holds its last key (`Cycle Type` is Clamp
/// for every door sequence in the install), and the doorway is wide enough to walk through here.
pub const OPEN_FRACTION: f32 = 0.5;

/// How far an `Open` clip has to turn a door's leaf, in degrees, before the swing counts as having
/// taken the leaf out of the doorway.
///
/// This is what decides whether an open door is a doorway or a wall with a leaf in it. Most Skyrim
/// doors swing far past it (an Imperial or Nordic door turns 115-135 degrees), but some barely
/// move: the demo route's `DweDoorLarge01Load` turns its two leaves 5.4 and 8.7 degrees, which
/// leaves the doorway as closed as it was. A leaf that turned less than this is hidden once the
/// door is [`Open`](DoorState::Open) - the same opening a static door gets - while one that swung
/// clear stays drawn, swung open.
pub const DOORWAY_CLEAR_DEGREES: f32 = 45.0;

/// Where a load door's leaf is in its own animation, driven by
/// [`DoorAnimationPlugin`](crate::door_animation::DoorAnimationPlugin) and read by everything that
/// has to agree with it: [`is_open`](DoorState::is_open) is what the portal and the crossing gate
/// on, and [`hides_whole_reference`](DoorState::hides_whole_reference) is what the
/// portal hides a static door with.
///
/// A load door is born [`Closed`](DoorState::Closed). An auto-load door - an invisible marker with
/// no leaf - is born `Open { animated: false }` and never changes: crossing the player on contact
/// is the whole of its behaviour.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DoorState {
    /// Nothing has opened this door: the leaf is drawn and solid. The model's rest pose is the pose
    /// its `Open` clip starts from, so this is also what a door that just streamed in looks like.
    #[default]
    Closed,
    /// The `Open` clip is playing, visibly: the leaf swings. The doorway is usable from the first
    /// frame of the swing, so the portal may render the destination through it and the crossing may
    /// be armed before the swing finishes.
    Opening,
    /// The door has opened: the `Open` clip reached [`OPEN_FRACTION`] and holds its last key, or -
    /// with `animated` false - the door has no clip of its own and was promoted here in the frame
    /// it was activated.
    Open {
        /// Whether the door has an animation of its own. A door without one has no leaf that can
        /// move out of the way, so an open one is a hole where its model was
        /// ([`hides_whole_reference`](DoorState::hides_whole_reference)); a door with one keeps its
        /// frame, and its leaves are drawn or hidden by where the swing left them.
        animated: bool,
    },
    /// The `Close` clip is playing: the leaf is coming back into the doorway, drawn, and solid
    /// again.
    Closing,
}

impl DoorState {
    /// Whether the doorway may be used in this state: the door has been asked to open and its leaf
    /// is on its way out of the opening, so the portal may render the destination through it and
    /// the crossing may be armed. True while the door is [`Opening`](Self::Opening) or
    /// [`Open`](Self::Open), false while it is [`Closed`](Self::Closed) or
    /// [`Closing`](Self::Closing).
    ///
    /// This is a property of the door state and not of the portal: a door half a unit from the eye
    /// is still an open door, and drawing its leaf back in there is the pop the design note's
    /// section 4.7 row 1 is about.
    pub fn is_open(self) -> bool {
        matches!(self, DoorState::Opening | DoorState::Open { .. })
    }

    /// Whether the whole door model has to be hidden for the doorway to be a hole - true only for
    /// an open door that has no animation of its own to move its leaf out of the way.
    ///
    /// An animated door never answers true: its frame is the doorway, and its leaves are a separate
    /// question ([`crate::door_animation`] hides the leaves of a door whose `Open` clip does not
    /// clear the opening, and leaves the ones that swung clear drawn).
    pub fn hides_whole_reference(self) -> bool {
        matches!(self, DoorState::Open { animated: false })
    }
}

/// The part of a load door's model its own animation moves: the nodes of the spawned glTF scene
/// with curves in the door's `Open` or `Close` clip, and therefore the leaf rather than the frame
/// and arch around it.
///
/// [`crate::door_animation`] marks them when it attaches a door's animation, and hides the leaves a
/// [`Open`](DoorState::Open) door's clip did not swing clear. [`mesh_is_out_of_the_way`] is how the
/// walk probe uses them: a leaf mid-swing is drawn, and it must not block the doorway or trap a
/// player walking through a door that is still opening ([`crate::player`]'s ray-cast filter).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct DoorLeaf {
    /// The load door reference this leaf belongs to: the entity carrying [`LoadDoor`] and
    /// [`DoorState`].
    pub door: Entity,
}

/// Whether a mesh the walk probe hit is part of a load door leaf its door has taken out of the
/// doorway - the probe skips it, and the doorway stays walkable while the door is open.
///
/// A probe hits a mesh primitive, which sits one or more levels below the marked node, so this
/// walks up from `hit` until it finds a [`DoorLeaf`] and answers from that door's [`DoorState`].
/// Anything that is not under a marked node - the frame, the wall, the floor - is not a leaf.
pub fn mesh_is_out_of_the_way(
    hit: Entity,
    parents: &Query<&ChildOf>,
    leaves: &Query<&DoorLeaf>,
    states: &Query<&DoorState>,
) -> bool {
    let mut entity = Some(hit);
    while let Some(current) = entity {
        if let Ok(leaf) = leaves.get(current) {
            return states.get(leaf.door).is_ok_and(|state| state.is_open());
        }
        entity = parents.get(current).ok().map(ChildOf::parent);
    }
    false
}

/// Request to go through a load door on the spot: the crossing snaps the camera to the link's
/// `XTEL` arrival point and nothing else happens. Written by a scripted run (`--demo-tour`) and by
/// tests; read by [`crate::transition`], which ignores entities that are gone or carry no
/// [`LoadDoor`].
///
/// It is deliberately *not* what `E` writes and not what opens a door: a player who presses `E`
/// writes [`OpenDoor`](crate::transition::OpenDoor), [`crate::door_animation`] runs the door's own
/// swing for it, and the crossing is the player walking through the doorway. A scripted run has no
/// player to walk, so it snaps - and a snap must not start an animation, whose swing would be of a
/// door the camera has already left.
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

    /// A placed door reference for the tier rules: a doorway box, a model and a convention, with
    /// everything else at the values a wall door of a house has.
    fn placement(model: &str, rotation: f32, convention: Option<f32>) -> DoorwayPlacement {
        DoorwayPlacement {
            position: [0.0, 0.0, 0.0],
            rotation: [0.0, 0.0, rotation],
            scale: 1.0,
            box_centre: Some([0.0, 88.0, -13.5]),
            model: model.to_owned(),
            convention,
            grid: Some([5, 4]),
            return_arrival_z: Some(0.0),
        }
    }

    /// Which tier the data buys, and what the anchor carries for it
    /// (`docs/research/portal-door-alignment.md` section 9.2): the same base model anchors with no
    /// convention at all, two different models need both of theirs, and a facing that cannot be
    /// established falls back to the doorway *centres* with today's facings.
    #[test]
    fn doorway_anchors_take_the_tier_the_data_supports() {
        let arrival = [0.0, 0.0, 0.0];

        // One model on both sides, neither convention known: tier 1, and what it can say about the
        // two doorways' facings is their *difference* - the conventions cancel out of it.
        let source = placement("farmhouse/door.nif", 1.0, None);
        let destination = placement("farmhouse/door.nif", -1.0, None);
        let anchor = doorway_anchor(&source, &destination, arrival, false).unwrap();
        assert_eq!(anchor.tier, DoorAnchorTier::SameModel);
        assert_eq!(
            anchor.facings,
            DoorwayFacings::SameModel { turn: -2.0 },
            "the destination doorway faces -1.0 and the source 1.0, whatever the model's axis is"
        );
        assert_eq!(anchor.source_box_centre, [0.0, 88.0, -13.5]);
        assert_eq!(anchor.destination.box_centre, [0.0, 88.0, -13.5]);
        assert_eq!(anchor.destination_grid, Some([5, 4]));

        // The same pair once both conventions are known: the facings are absolute now, and their
        // difference - the map's turn - is the one the cancellation promised.
        let mut known_source = source.clone();
        known_source.convention = Some(0.7);
        let mut known_destination = destination.clone();
        known_destination.convention = Some(0.7);
        let known = doorway_anchor(&known_source, &known_destination, arrival, false).unwrap();
        assert_eq!(known.tier, DoorAnchorTier::SameModel);
        assert_eq!(
            known.facings,
            DoorwayFacings::Known {
                source: 1.7,
                destination: -0.3,
            }
        );
        let DoorwayFacings::Known {
            source: known_source_facing,
            destination: known_destination_facing,
        } = known.facings
        else {
            unreachable!()
        };
        assert_eq!(
            known_destination_facing - known_source_facing,
            -2.0,
            "which is exactly the turn the pair had with no convention at all"
        );

        // Different models whose placements agree on an axis each: tier 2, yaw plus convention.
        let source = placement("farmhouse/door.nif", 1.0, Some(0.5));
        let destination = placement("nordic/door.nif", -1.0, Some(3.0));
        let anchor = doorway_anchor(&source, &destination, arrival, false).unwrap();
        assert_eq!(anchor.tier, DoorAnchorTier::Conventions);
        assert_eq!(
            anchor.facings,
            DoorwayFacings::Known {
                source: 1.5,
                destination: 2.0,
            }
        );

        // Different models, one of them placed both ways: tier 3 - the centres, today's facings.
        let destination = placement("nordic/door.nif", -1.0, None);
        let anchor = doorway_anchor(&source, &destination, arrival, false).unwrap();
        assert_eq!(anchor.tier, DoorAnchorTier::Centres);
        assert_eq!(anchor.facings, DoorwayFacings::Kept);
        assert_eq!(
            anchor.destination.box_centre,
            [0.0, 88.0, -13.5],
            "the centre is still the doorway's own"
        );

        // Two doors with no model at all are not "the same model": there is nothing to cancel.
        let source = placement("", 1.0, None);
        let destination = placement("", -1.0, None);
        assert_eq!(
            doorway_anchor(&source, &destination, arrival, false)
                .expect("the centres are still there to anchor on")
                .tier,
            DoorAnchorTier::Centres
        );
    }

    /// A doorway box on both sides and a landing that is at the doorway are what the anchor is for;
    /// everything else keeps today's map (the report's tier 4, and its section 9.4 for why).
    #[test]
    fn a_doorway_anchor_needs_a_doorway_and_a_landing_at_it() {
        let arrival = [0.0, 0.0, 0.0];
        let source = placement("farmhouse/door.nif", 1.0, None);
        let destination = placement("farmhouse/door.nif", -1.0, None);
        assert!(doorway_anchor(&source, &destination, arrival, false).is_some());

        // No bounds box on either side: an invisible marker, which has no doorway to line up.
        let mut one_sided = source.clone();
        one_sided.box_centre = None;
        assert_eq!(
            doorway_anchor(&one_sided, &destination, arrival, false),
            None
        );
        let mut other_sided = destination.clone();
        other_sided.box_centre = None;
        assert_eq!(doorway_anchor(&source, &other_sided, arrival, false), None);

        // A door nothing leads back to: the player's own floor level at the source is the one term
        // that cannot be measured, so the landing cannot be checked.
        let mut one_way = source.clone();
        one_way.return_arrival_z = None;
        assert_eq!(doorway_anchor(&one_way, &destination, arrival, false), None);

        // An exterior destination with no resolved cell: nothing to stream the landing's grid for.
        let mut gridless = destination.clone();
        gridless.grid = None;
        assert_eq!(doorway_anchor(&source, &gridless, arrival, true), None);
        assert!(
            doorway_anchor(&source, &gridless, arrival, false).is_some(),
            "an interior destination needs no grid of its own"
        );

        // The game's arrival 600 units away in plan is not a landing in the doorway.
        let mut far = destination.clone();
        far.position = [600.0, 0.0, 0.0];
        assert_eq!(doorway_anchor(&source, &far, arrival, false), None);
    }

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

    /// A link that arrives hundreds of units away is pointing into the room the door opens onto,
    /// not at the door: the direction from the door to the far end of that room is not the
    /// direction the door faces, so the heading says which way it faces instead.
    #[test]
    fn an_arrival_point_far_from_the_door_leaves_the_heading_to_say_which_way_it_faces() {
        // 600 units west of the door, with a heading of 92 degrees (east): the point says west and
        // the heading says east, and east is the answer - the same one the point gives at 32 units
        // in `the_arrival_point_of_the_return_link_is_the_way_the_door_faces`.
        let far_west = [TOWER_DOOR[0] - 600.0, TOWER_DOOR[1], TOWER_DOOR[2]];
        let outward = outward_from_return_link(TOWER_DOOR, far_west, TOWER_HEADING).unwrap();
        assert!(
            outward[0] > 0.99 && outward[1].abs() < 0.05,
            "600 units away the heading decides: {outward:?}"
        );

        // The boundary itself: a point at [`OUTWARD_POINT_MAX_DISTANCE`] is still at the door, and
        // one unit further out is not.
        let at = |east: f32| [TOWER_DOOR[0] + east, TOWER_DOOR[1], TOWER_DOOR[2]];
        let south = [0.0, 0.0, 180.0_f32.to_radians()];
        let outward =
            outward_from_return_link(TOWER_DOOR, at(OUTWARD_POINT_MAX_DISTANCE), TOWER_HEADING)
                .unwrap();
        assert!(outward[0] > 0.99, "the last trusted point: {outward:?}");
        let outward =
            outward_from_return_link(TOWER_DOOR, at(OUTWARD_POINT_MAX_DISTANCE + 1.0), south)
                .unwrap();
        assert!(
            outward[0].abs() < 0.05 && outward[1] < -0.99,
            "one unit past it the heading does: {outward:?}"
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
