//! Coordinate-system contract shared by converted assets and the runtime.

/// Quaternion rotating Creation Engine/NIF Z-up coordinates into glTF/Bevy
/// Y-up coordinates. Components are `[x, y, z, w]`.
pub const CREATION_TO_RUNTIME_ROTATION: [f32; 4] = [
    -std::f32::consts::FRAC_1_SQRT_2,
    0.0,
    0.0,
    std::f32::consts::FRAC_1_SQRT_2,
];

/// Maps a point or direction from Creation Engine coordinates into runtime
/// coordinates. This is the same basis represented by
/// [`CREATION_TO_RUNTIME_ROTATION`].
pub const fn creation_to_runtime_vector([x, y, z]: [f32; 3]) -> [f32; 3] {
    [x, z, -y]
}

/// Inverse of [`creation_to_runtime_vector`].
pub const fn runtime_to_creation_vector([x, y, z]: [f32; 3]) -> [f32; 3] {
    [x, -z, y]
}

/// Converts Skyrim `REFR/DATA` XYZ Euler angles (radians) into the quaternion
/// used by glTF/Bevy.
///
/// Creation angles turn **clockwise** about each axis (seen looking down the
/// axis), the Gamebryo convention of a transposed rotation matrix: the stored
/// angles describe `Rz * Ry * Rx`, and the object is rotated by its inverse,
/// `Rx(-x) * Ry(-y) * Rz(-z)` (the same as OpenMW does for Bethesda references).
/// For a yaw-only reference that is `Rz(-z)`: a heading measured clockwise from
/// north, the same as an `XTEL` arrival heading. Measured on Skyrim.esm's load
/// doors (2026-09-22): for 63 door models with 5+ placements, 88% of doors put
/// their `XTEL` arrival point at their model's usual angle under this
/// convention, against 42% under the counter-clockwise one used until then.
/// The result is conjugated by the Creation-to-runtime basis.
pub fn creation_euler_to_runtime_quaternion([x, y, z]: [f32; 3]) -> [f32; 4] {
    let source = conjugate_quaternion(multiply_quaternions(
        axis_angle([0.0, 0.0, 1.0], z),
        multiply_quaternions(
            axis_angle([0.0, 1.0, 0.0], y),
            axis_angle([1.0, 0.0, 0.0], x),
        ),
    ));
    let basis = CREATION_TO_RUNTIME_ROTATION;
    normalize_quaternion(multiply_quaternions(
        multiply_quaternions(basis, source),
        conjugate_quaternion(basis),
    ))
}

fn axis_angle(axis: [f32; 3], angle: f32) -> [f32; 4] {
    let half = angle * 0.5;
    let sine = half.sin();
    [axis[0] * sine, axis[1] * sine, axis[2] * sine, half.cos()]
}

fn multiply_quaternions(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    [
        a[3] * b[0] + a[0] * b[3] + a[1] * b[2] - a[2] * b[1],
        a[3] * b[1] - a[0] * b[2] + a[1] * b[3] + a[2] * b[0],
        a[3] * b[2] + a[0] * b[1] - a[1] * b[0] + a[2] * b[3],
        a[3] * b[3] - a[0] * b[0] - a[1] * b[1] - a[2] * b[2],
    ]
}

fn conjugate_quaternion([x, y, z, w]: [f32; 4]) -> [f32; 4] {
    [-x, -y, -z, w]
}

fn normalize_quaternion(quaternion: [f32; 4]) -> [f32; 4] {
    let length = quaternion
        .iter()
        .map(|value| value * value)
        .sum::<f32>()
        .sqrt();
    if length == 0.0 || !length.is_finite() {
        return [0.0, 0.0, 0.0, 1.0];
    }
    quaternion.map(|value| value / length)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rotate(q: [f32; 4], vector: [f32; 3]) -> [f32; 3] {
        let vector_q = [vector[0], vector[1], vector[2], 0.0];
        let rotated =
            multiply_quaternions(multiply_quaternions(q, vector_q), conjugate_quaternion(q));
        [rotated[0], rotated[1], rotated[2]]
    }

    fn assert_close(actual: [f32; 3], expected: [f32; 3]) {
        for axis in 0..3 {
            assert!(
                (actual[axis] - expected[axis]).abs() < 1.0e-5,
                "axis {axis}: {actual:?} != {expected:?}"
            );
        }
    }

    #[test]
    fn basis_maps_creation_axes_to_runtime_axes() {
        assert_eq!(creation_to_runtime_vector([1.0, 0.0, 0.0]), [1.0, 0.0, 0.0]);
        assert_eq!(
            creation_to_runtime_vector([0.0, 1.0, 0.0]),
            [0.0, 0.0, -1.0]
        );
        assert_eq!(creation_to_runtime_vector([0.0, 0.0, 1.0]), [0.0, 1.0, 0.0]);
    }

    #[test]
    fn basis_round_trip_preserves_vector() {
        let source = [123.5, -42.25, 0.125];
        assert_eq!(
            runtime_to_creation_vector(creation_to_runtime_vector(source)),
            source
        );
    }

    #[test]
    fn identity_euler_stays_identity_after_basis_change() {
        assert_close(
            rotate(
                creation_euler_to_runtime_quaternion([0.0; 3]),
                [1.0, 2.0, 3.0],
            ),
            [1.0, 2.0, 3.0],
        );
    }

    #[test]
    fn creation_z_rotation_becomes_runtime_y_rotation() {
        // Clockwise seen from above: a quarter turn takes Creation +X (east) to -Y (south),
        // which is runtime +Z.
        let runtime = creation_euler_to_runtime_quaternion([0.0, 0.0, std::f32::consts::FRAC_PI_2]);
        assert_close(rotate(runtime, [1.0, 0.0, 0.0]), [0.0, 0.0, 1.0]);
    }

    #[test]
    fn rotations_about_creation_x_and_y_map_to_runtime_axes() {
        let quarter = std::f32::consts::FRAC_PI_2;
        // Clockwise about Creation X: +Y (runtime -Z) turns to -Z (runtime -Y).
        let x_rotation = creation_euler_to_runtime_quaternion([quarter, 0.0, 0.0]);
        assert_close(rotate(x_rotation, [0.0, 0.0, -1.0]), [0.0, -1.0, 0.0]);

        // Clockwise about Creation Y: +X turns to +Z (runtime +Y).
        let y_rotation = creation_euler_to_runtime_quaternion([0.0, quarter, 0.0]);
        assert_close(rotate(y_rotation, [1.0, 0.0, 0.0]), [0.0, 1.0, 0.0]);
    }

    #[test]
    fn converted_rotation_matches_converted_source_vector() {
        let source_rotation = creation_euler_to_runtime_quaternion([0.3, -0.7, 1.1]);
        let source_vector = [2.0, -3.0, 5.0];

        let qx = axis_angle([1.0, 0.0, 0.0], 0.3);
        let qy = axis_angle([0.0, 1.0, 0.0], -0.7);
        let qz = axis_angle([0.0, 0.0, 1.0], 1.1);
        let rotated_creation = rotate(
            conjugate_quaternion(multiply_quaternions(qz, multiply_quaternions(qy, qx))),
            source_vector,
        );
        assert_close(
            rotate(source_rotation, creation_to_runtime_vector(source_vector)),
            creation_to_runtime_vector(rotated_creation),
        );
    }

    #[test]
    fn a_yaw_turns_clockwise_like_a_heading() {
        // A door at yaw z looks along Creation (sin z, cos z): the heading convention.
        for z in [0.4_f32, 1.3, 2.9, -1.8] {
            let q = creation_euler_to_runtime_quaternion([0.0, 0.0, z]);
            let forward = rotate(q, creation_to_runtime_vector([0.0, 1.0, 0.0]));
            assert_close(forward, creation_to_runtime_vector([z.sin(), z.cos(), 0.0]));
        }
    }
}
