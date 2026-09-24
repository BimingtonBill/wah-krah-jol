//! Skyrim's billboard nodes: model parts that turn to face the camera, such as a hearth's flame
//! cards (`FireplaceWood01Burning`'s `Flames` node).
//!
//! A NIF marks them with an `NiBillboardNode`, which the converter publishes as node extras,
//! `{"openSkyrim": {"billboard": "<mode>"}}`, the mode being nif.xml's `BillboardMode` name in
//! camelCase (Phase 2 Dev's `phase2/nif-billboard-nodes`, 2026-09-25). Without the turn a card
//! keeps its authored angle and is seen edge-on or from behind from most places in a room.
//!
//! Two behaviours cover the modes:
//! * **Face the camera** (`alwaysFaceCamera`, `rigidFaceCamera`, `alwaysFaceCenter`,
//!   `rigidFaceCenter`): the node takes the camera's orientation, as NifSkope draws them (it sets
//!   the node's rotation in view space to identity). A card authored in the node's XY plane then
//!   faces the viewer upright, which is Gamebryo's camera convention and Bevy's.
//! * **Turn about up** (`rotateAboutUp`, `rotateAboutUp2`, `bsRotateAboutUp`): the node keeps its
//!   authored tilt and turns about the world's up axis until its facing axis (node-local Z) points
//!   at the camera across the ground plane. Flames stay upright however the camera looks down.
//!
//! The camera is the player's, [`StreamingCamera`]; a portal's view of another space sees the
//! cards turned toward the player, which from a doorway is nearly the same direction.

use bevy::{gltf::GltfExtras, prelude::*, transform::TransformSystems};

use crate::world::components::StreamingCamera;

/// How a billboard node turns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BillboardMode {
    FaceCamera,
    TurnAboutUp,
}

impl BillboardMode {
    /// The behaviour for a published mode name; `None` for a name this engine does not know.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "alwaysFaceCamera" | "rigidFaceCamera" | "alwaysFaceCenter" | "rigidFaceCenter" => {
                Some(Self::FaceCamera)
            }
            "rotateAboutUp" | "rotateAboutUp2" | "bsRotateAboutUp" => Some(Self::TurnAboutUp),
            _ => None,
        }
    }

    /// The mode a node's glTF extras publish, if any.
    pub fn from_extras(extras: &str) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(extras).ok()?;
        Self::from_name(value.get("openSkyrim")?.get("billboard")?.as_str()?)
    }
}

/// A node that turns to face the camera, with the local rotation it was authored with.
#[derive(Component, Debug, Clone, Copy)]
pub struct Billboard {
    pub mode: BillboardMode,
    pub authored: Quat,
}

pub struct BillboardPlugin;

impl Plugin for BillboardPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, find_billboards).add_systems(
            PostUpdate,
            turn_billboards.before(TransformSystems::Propagate),
        );
    }
}

/// A glTF node whose extras have just arrived and that is not yet known to be a billboard.
type NewNode<'a> = (Entity, &'a GltfExtras, &'a Transform);

fn find_billboards(
    mut commands: Commands,
    nodes: Query<NewNode, (Added<GltfExtras>, Without<Billboard>)>,
) {
    for (entity, extras, transform) in &nodes {
        if let Some(mode) = BillboardMode::from_extras(&extras.value) {
            commands.entity(entity).try_insert(Billboard {
                mode,
                authored: transform.rotation,
            });
        }
    }
}

/// The world rotation a billboard takes: `authored` is its world rotation as placed, `to_camera`
/// the direction from it to the camera and `camera` the camera's world rotation, all in Bevy's
/// Y-up world.
pub fn billboard_rotation(
    mode: BillboardMode,
    authored: Quat,
    to_camera: Vec3,
    camera: Quat,
) -> Quat {
    match mode {
        BillboardMode::FaceCamera => camera,
        BillboardMode::TurnAboutUp => {
            let facing = authored * Vec3::Z;
            let from = Vec2::new(facing.x, facing.z);
            let to = Vec2::new(to_camera.x, to_camera.z);
            if from.length_squared() < 1e-8 || to.length_squared() < 1e-8 {
                return authored;
            }
            // The turn about +Y that carries `from` onto `to` in the XZ plane. A rotation about +Y
            // by `a` takes (x, z) to (x cos a + z sin a, -x sin a + z cos a), so the angle is
            // measured from `to` back to `from`.
            let angle = to.angle_to(from);
            Quat::from_rotation_y(angle) * authored
        }
    }
}

fn turn_billboards(
    camera: Query<&GlobalTransform, With<StreamingCamera>>,
    mut billboards: Query<(&Billboard, &mut Transform, &ChildOf, &GlobalTransform)>,
    parents: Query<&GlobalTransform>,
) {
    let Ok(camera) = camera.single() else {
        return;
    };
    let (_, camera_rotation, camera_position) = camera.to_scale_rotation_translation();
    for (billboard, mut transform, child_of, global) in &mut billboards {
        let Ok(parent) = parents.get(child_of.parent()) else {
            continue;
        };
        let (_, parent_rotation, _) = parent.to_scale_rotation_translation();
        let authored_world = parent_rotation * billboard.authored;
        let world = billboard_rotation(
            billboard.mode,
            authored_world,
            camera_position - global.translation(),
            camera_rotation,
        );
        let local = parent_rotation.inverse() * world;
        if transform.rotation != local {
            transform.rotation = local;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_names_map_to_the_two_behaviours() {
        assert_eq!(
            BillboardMode::from_extras(r#"{"openSkyrim":{"billboard":"rotateAboutUp"}}"#),
            Some(BillboardMode::TurnAboutUp)
        );
        assert_eq!(
            BillboardMode::from_name("alwaysFaceCamera"),
            Some(BillboardMode::FaceCamera)
        );
        assert_eq!(
            BillboardMode::from_extras(r#"{"openSkyrim":{"billboard":7}}"#),
            None
        );
        assert_eq!(BillboardMode::from_extras(r#"{"other":1}"#), None);
    }

    #[test]
    fn turning_about_up_points_the_facing_axis_at_the_camera_and_keeps_it_upright() {
        // Authored facing +Z; the camera stands off along +X and above.
        let to_camera = Vec3::new(10.0, 4.0, 0.0);
        let world = billboard_rotation(
            BillboardMode::TurnAboutUp,
            Quat::IDENTITY,
            to_camera,
            Quat::IDENTITY,
        );
        let facing = world * Vec3::Z;
        assert!((facing - Vec3::X).length() < 1e-5, "{facing}");
        assert!(
            ((world * Vec3::Y) - Vec3::Y).length() < 1e-5,
            "stays upright"
        );

        // A tilted card keeps its tilt: only a turn about +Y is added.
        let tilted = Quat::from_rotation_x(0.3);
        let world = billboard_rotation(
            BillboardMode::TurnAboutUp,
            tilted,
            Vec3::new(-5.0, 0.0, 0.0),
            Quat::IDENTITY,
        );
        let facing = world * Vec3::Z;
        let flat = Vec2::new(facing.x, facing.z).normalize();
        assert!((flat - Vec2::new(-1.0, 0.0)).length() < 1e-5, "{facing}");
        assert!(((world * Vec3::X).y).abs() < 1e-5, "no roll is added");
    }

    #[test]
    fn facing_the_camera_takes_its_rotation() {
        let camera = Quat::from_euler(EulerRot::YXZ, 1.0, -0.4, 0.0);
        let world = billboard_rotation(BillboardMode::FaceCamera, Quat::IDENTITY, Vec3::X, camera);
        assert_eq!(world, camera);
    }
}
