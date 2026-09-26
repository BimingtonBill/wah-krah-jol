//! The portal graph: every load door of the world, the space it stands in and the space it leads
//! into, held as one resource for the whole run.
//!
//! Spaces are the nodes (a worldspace, or an interior cell) and load doors are the directed edges,
//! read once from `door_links` and the door references when the world database opens. The queries
//! answer what a one-door-at-a-time view cannot: which doors lead into the same interior (the basis
//! for sharing one destination between them), which spaces are a few crossings away, and which
//! nearby door the player is most likely to use next.
//!
//! Nothing reads the graph yet: it is the foundation for a shared destination per interior and for
//! predictive streaming across every nearby door (`local/research/brainstorm-208-*`, idea 1).

use crate::config::EngineConfig;
use bevy::prelude::*;
use color_eyre::Result;
use rusqlite::{Connection, OpenFlags};
use std::collections::{HashMap, HashSet, VecDeque};
use std::f32::consts::PI;
use std::path::Path;

/// A node of the graph: a place the camera can stand in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Space {
    /// An exterior worldspace, by its FormID.
    Worldspace(u32),
    /// An interior cell, by its FormID.
    Interior(u32),
}

/// An edge of the graph: one load door, where it stands and where it leads.
#[derive(Debug, Clone, PartialEq)]
pub struct PortalEdge {
    /// The door reference's FormID.
    pub door_ref: u32,
    /// The space the door stands in.
    pub from: Space,
    /// The door reference's position, in Creation-engine units.
    pub position: [f32; 3],
    /// The door the link leads to (`XTEL` bytes 0..4).
    pub destination_ref: u32,
    /// The space the link leads into; `None` when the converter could not resolve it.
    pub to: Option<Space>,
    /// Where a crossing lands, in Creation-engine units (`XTEL` bytes 4..16).
    pub arrival_position: [f32; 3],
    /// Which way a crossing faces, in Creation-engine radians (`XTEL` bytes 16..28).
    pub arrival_rotation: [f32; 3],
}

/// Counts that describe the graph, logged once at startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PortalGraphSummary {
    /// Every space that has a door in it or a door leading into it.
    pub spaces: usize,
    /// Every load door.
    pub doors: usize,
    /// Doors whose link leads into an interior.
    pub links_into_interiors: usize,
    /// Interiors at least one door leads into.
    pub interiors_reached: usize,
    /// Interiors more than one door leads into.
    pub interiors_with_several_doors: usize,
    /// Doors whose destination the converter could not resolve.
    pub unresolved: usize,
}

/// How much a door's angle off the heading costs in ranking, in Creation units per radian: a door
/// straight behind the player (pi radians) ranks as if it were about 800 units farther away than
/// one straight ahead. Tuned by nothing yet; a starting value for the predictive streaming.
pub const ANGLE_COST_UNITS_PER_RADIAN: f32 = 256.0;

/// Every load door of the world, indexed by the space it stands in and the space it leads into.
#[derive(Resource, Debug, Clone, Default)]
pub struct PortalGraph {
    edges: Vec<PortalEdge>,
    /// Edge indices by the space the door stands in.
    out_of: HashMap<Space, Vec<usize>>,
    /// Edge indices by the space the door leads into.
    into: HashMap<Space, Vec<usize>>,
}

impl PortalGraph {
    /// A graph over these doors.
    pub fn from_edges(edges: Vec<PortalEdge>) -> Self {
        let mut out_of: HashMap<Space, Vec<usize>> = HashMap::new();
        let mut into: HashMap<Space, Vec<usize>> = HashMap::new();
        for (index, edge) in edges.iter().enumerate() {
            out_of.entry(edge.from).or_default().push(index);
            if let Some(to) = edge.to {
                into.entry(to).or_default().push(index);
            }
        }
        Self {
            edges,
            out_of,
            into,
        }
    }

    /// Reads the graph from a converted world database, read-only. A database without a
    /// `door_links` table (one converted before doors, or the streaming fixture's) is an empty graph.
    pub fn open(path: &Path) -> Result<Self> {
        let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        Self::read(&connection)
    }

    /// Reads the graph from an open world database connection.
    ///
    /// A door stands in its cell's worldspace, or in the cell itself when the cell has none (an
    /// interior). A link leads into its destination worldspace when it names one, else into its
    /// destination cell; neither is an unresolved link.
    pub fn read(connection: &Connection) -> Result<Self> {
        let has_links: i64 = connection.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='door_links'",
            [],
            |row| row.get(0),
        )?;
        if has_links == 0 {
            return Ok(Self::default());
        }
        let mut statement = connection.prepare(
            "SELECT d.ref_id,r.cell_id,c.worldspace_id,r.pos_x,r.pos_y,r.pos_z,\
             d.destination_ref_id,d.destination_cell_id,d.destination_worldspace_id,\
             d.pos_x,d.pos_y,d.pos_z,d.rot_x,d.rot_y,d.rot_z \
             FROM door_links d JOIN \"references\" r ON r.id=d.ref_id \
             LEFT JOIN cells c ON c.id=r.cell_id ORDER BY d.ref_id",
        )?;
        let mut rows = statement.query([])?;
        let mut edges = Vec::new();
        while let Some(row) = rows.next()? {
            let cell_id: u32 = row.get(1)?;
            let from = match row.get::<_, Option<u32>>(2)? {
                Some(worldspace_id) => Space::Worldspace(worldspace_id),
                None => Space::Interior(cell_id),
            };
            let to = match (row.get::<_, Option<u32>>(8)?, row.get::<_, Option<u32>>(7)?) {
                (Some(worldspace_id), _) => Some(Space::Worldspace(worldspace_id)),
                (None, Some(cell_id)) => Some(Space::Interior(cell_id)),
                (None, None) => None,
            };
            edges.push(PortalEdge {
                door_ref: row.get(0)?,
                from,
                position: [row.get(3)?, row.get(4)?, row.get(5)?],
                destination_ref: row.get(6)?,
                to,
                arrival_position: [row.get(9)?, row.get(10)?, row.get(11)?],
                arrival_rotation: [row.get(12)?, row.get(13)?, row.get(14)?],
            });
        }
        Ok(Self::from_edges(edges))
    }

    /// Every door of the graph.
    pub fn edges(&self) -> &[PortalEdge] {
        &self.edges
    }

    /// The door with this reference FormID.
    pub fn door(&self, door_ref: u32) -> Option<&PortalEdge> {
        self.edges.iter().find(|edge| edge.door_ref == door_ref)
    }

    /// Query 1: the doors that stand in a space.
    pub fn doors_in(&self, space: Space) -> impl Iterator<Item = &PortalEdge> {
        self.indexed(self.out_of.get(&space))
    }

    /// Query 2: the doors of a space within `radius` Creation units of `position` (straight-line
    /// distance), nearest first.
    pub fn doors_near(
        &self,
        space: Space,
        position: [f32; 3],
        radius: f32,
    ) -> Vec<(&PortalEdge, f32)> {
        let mut near: Vec<(&PortalEdge, f32)> = self
            .doors_in(space)
            .map(|edge| (edge, distance(edge.position, position)))
            .filter(|(_, distance)| *distance <= radius)
            .collect();
        near.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.door_ref.cmp(&b.0.door_ref)));
        near
    }

    /// Query 3: every door that leads into a space, from whichever space it stands in. What a
    /// destination shared between the several doors of one interior is keyed on.
    pub fn doors_into(&self, space: Space) -> impl Iterator<Item = &PortalEdge> {
        self.indexed(self.into.get(&space))
    }

    /// Query 4: every space reachable from `start` in at most `max_crossings` door crossings, with
    /// the fewest crossings that reach it. `start` is in the map at 0.
    pub fn reachable_within(&self, start: Space, max_crossings: u32) -> HashMap<Space, u32> {
        let mut reached = HashMap::from([(start, 0)]);
        let mut queue = VecDeque::from([(start, 0u32)]);
        while let Some((space, crossings)) = queue.pop_front() {
            if crossings >= max_crossings {
                continue;
            }
            for edge in self.doors_in(space) {
                let Some(to) = edge.to else { continue };
                if let std::collections::hash_map::Entry::Vacant(entry) = reached.entry(to) {
                    entry.insert(crossings + 1);
                    queue.push_back((to, crossings + 1));
                }
            }
        }
        reached
    }

    /// Query 5: the doors of a space within `radius` of `position`, ranked by how likely the player
    /// is to use one next - most likely first. The score is the distance plus
    /// [`ANGLE_COST_UNITS_PER_RADIAN`] times the horizontal angle between `forward` (the heading,
    /// a Creation-engine x/y direction; its length does not matter) and the direction to the door.
    /// A zero `forward` ranks by distance alone.
    pub fn rank_next_doors(
        &self,
        space: Space,
        position: [f32; 3],
        forward: [f32; 2],
        radius: f32,
    ) -> Vec<(&PortalEdge, f32)> {
        let mut ranked: Vec<(&PortalEdge, f32)> = self
            .doors_near(space, position, radius)
            .into_iter()
            .map(|(edge, distance)| {
                let to_door = [
                    edge.position[0] - position[0],
                    edge.position[1] - position[1],
                ];
                let angle = horizontal_angle(forward, to_door);
                (edge, distance + ANGLE_COST_UNITS_PER_RADIAN * angle)
            })
            .collect();
        ranked.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.door_ref.cmp(&b.0.door_ref)));
        ranked
    }

    /// The graph's counts.
    pub fn summary(&self) -> PortalGraphSummary {
        let mut spaces: HashSet<Space> = self.out_of.keys().copied().collect();
        spaces.extend(self.into.keys().copied());
        let interiors_into = self
            .into
            .iter()
            .filter(|(space, _)| matches!(space, Space::Interior(_)));
        let (mut interiors_reached, mut several, mut links) = (0, 0, 0);
        for (_, doors) in interiors_into {
            interiors_reached += 1;
            links += doors.len();
            if doors.len() > 1 {
                several += 1;
            }
        }
        PortalGraphSummary {
            spaces: spaces.len(),
            doors: self.edges.len(),
            links_into_interiors: links,
            interiors_reached,
            interiors_with_several_doors: several,
            unresolved: self.edges.iter().filter(|edge| edge.to.is_none()).count(),
        }
    }

    fn indexed<'a>(
        &'a self,
        indices: Option<&'a Vec<usize>>,
    ) -> impl Iterator<Item = &'a PortalEdge> + 'a {
        indices
            .into_iter()
            .flatten()
            .map(move |&index| &self.edges[index])
    }
}

fn distance(a: [f32; 3], b: [f32; 3]) -> f32 {
    let d = [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
    (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt()
}

/// The unsigned angle between two horizontal directions, in radians (0..=pi); 0 when either is
/// zero-length, so a standing player or a door underfoot costs nothing for angle.
fn horizontal_angle(a: [f32; 2], b: [f32; 2]) -> f32 {
    let length = (a[0].hypot(a[1])) * (b[0].hypot(b[1]));
    if length <= f32::EPSILON {
        return 0.0;
    }
    let cosine = ((a[0] * b[0] + a[1] * b[1]) / length).clamp(-1.0, 1.0);
    cosine.acos().clamp(0.0, PI)
}

/// Builds the [`PortalGraph`] from the run's world database and logs its summary. Added only to a
/// run that opened the world; a database the graph cannot read gives an empty graph and a warning,
/// never a failed run - nothing depends on the graph yet.
pub struct PortalGraphPlugin;

impl Plugin for PortalGraphPlugin {
    fn build(&self, app: &mut App) {
        let path = app
            .world()
            .resource::<EngineConfig>()
            .assets_dir
            .join("skyrim_world.db");
        let graph = match PortalGraph::open(&path) {
            Ok(graph) => graph,
            Err(error) => {
                warn!(%error, path = %path.display(), "portal graph: cannot read the door links");
                PortalGraph::default()
            }
        };
        let summary = graph.summary();
        info!(
            spaces = summary.spaces,
            doors = summary.doors,
            links_into_interiors = summary.links_into_interiors,
            interiors = summary.interiors_reached,
            interiors_with_several_doors = summary.interiors_with_several_doors,
            unresolved = summary.unresolved,
            "portal graph built"
        );
        app.insert_resource(graph);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TAMRIEL: Space = Space::Worldspace(60);
    const HOUSE: Space = Space::Interior(100);
    const CELLAR: Space = Space::Interior(200);
    const INN: Space = Space::Interior(300);

    fn edge(door_ref: u32, from: Space, position: [f32; 3], to: Option<Space>) -> PortalEdge {
        PortalEdge {
            door_ref,
            from,
            position,
            destination_ref: door_ref + 1000,
            to,
            arrival_position: [0.0; 3],
            arrival_rotation: [0.0; 3],
        }
    }

    /// Tamriel has a house with a front and a back door and an inn; the house has a trapdoor into
    /// a cellar; each interior has a door back out; one door in Tamriel is unresolved.
    fn fixture() -> PortalGraph {
        PortalGraph::from_edges(vec![
            edge(1, TAMRIEL, [1000.0, 0.0, 0.0], Some(HOUSE)),
            edge(2, TAMRIEL, [0.0, 1000.0, 0.0], Some(HOUSE)),
            edge(3, TAMRIEL, [-3000.0, 0.0, 0.0], Some(INN)),
            edge(4, TAMRIEL, [0.0, -500.0, 0.0], None),
            edge(10, HOUSE, [0.0, 0.0, 0.0], Some(TAMRIEL)),
            edge(11, HOUSE, [50.0, 0.0, 0.0], Some(CELLAR)),
            edge(20, CELLAR, [0.0, 0.0, -200.0], Some(HOUSE)),
            edge(30, INN, [0.0, 0.0, 0.0], Some(TAMRIEL)),
        ])
    }

    fn refs<'a>(edges: impl IntoIterator<Item = &'a PortalEdge>) -> Vec<u32> {
        let mut refs: Vec<u32> = edges.into_iter().map(|edge| edge.door_ref).collect();
        refs.sort_unstable();
        refs
    }

    #[test]
    fn doors_in_lists_the_doors_standing_in_a_space() {
        let graph = fixture();
        assert_eq!(refs(graph.doors_in(TAMRIEL)), [1, 2, 3, 4]);
        assert_eq!(refs(graph.doors_in(HOUSE)), [10, 11]);
        assert!(graph.doors_in(Space::Interior(999)).next().is_none());
    }

    #[test]
    fn doors_near_keeps_the_radius_and_sorts_nearest_first() {
        let graph = fixture();
        let near = graph.doors_near(TAMRIEL, [0.0, 0.0, 0.0], 1000.0);
        let order: Vec<u32> = near.iter().map(|(edge, _)| edge.door_ref).collect();
        // 4 at 500, then 1 and 2 at exactly 1000 (ties by FormID); 3 at 3000 is out.
        assert_eq!(order, [4, 1, 2]);
        assert!((near[0].1 - 500.0).abs() < 1e-3);
        assert!(graph.doors_near(HOUSE, [0.0, 0.0, 0.0], 10.0).len() == 1);
    }

    #[test]
    fn doors_into_finds_every_door_leading_into_a_space() {
        let graph = fixture();
        assert_eq!(refs(graph.doors_into(HOUSE)), [1, 2, 20]);
        assert_eq!(refs(graph.doors_into(TAMRIEL)), [10, 30]);
        assert_eq!(refs(graph.doors_into(CELLAR)), [11]);
    }

    #[test]
    fn reachable_within_counts_the_fewest_crossings() {
        let graph = fixture();
        assert_eq!(
            graph.reachable_within(TAMRIEL, 0),
            HashMap::from([(TAMRIEL, 0)])
        );
        assert_eq!(
            graph.reachable_within(TAMRIEL, 1),
            HashMap::from([(TAMRIEL, 0), (HOUSE, 1), (INN, 1)])
        );
        assert_eq!(
            graph.reachable_within(TAMRIEL, 2),
            HashMap::from([(TAMRIEL, 0), (HOUSE, 1), (INN, 1), (CELLAR, 2)])
        );
        assert_eq!(
            graph.reachable_within(CELLAR, 3),
            HashMap::from([(CELLAR, 0), (HOUSE, 1), (TAMRIEL, 2), (INN, 3)])
        );
    }

    #[test]
    fn rank_next_doors_prefers_the_door_ahead() {
        let graph = fixture();
        // Door 1 (east, 1000) and door 2 (north, 1000) are equally far; facing north, door 2
        // ranks first; facing east, door 1 does. Door 4 (south, 500) is nearer but behind.
        let north = graph.rank_next_doors(TAMRIEL, [0.0; 3], [0.0, 1.0], 1500.0);
        let order: Vec<u32> = north.iter().map(|(edge, _)| edge.door_ref).collect();
        assert_eq!(order, [2, 4, 1]);
        assert!((north[0].1 - 1000.0).abs() < 1e-2);
        assert!((north[1].1 - (500.0 + ANGLE_COST_UNITS_PER_RADIAN * PI)).abs() < 1e-2);
        // Facing east, door 1 now beats door 2; door 4, half as far and only a quarter turn off
        // (500 + 256 * pi/2 = 902), still beats both.
        let east = graph.rank_next_doors(TAMRIEL, [0.0; 3], [5.0, 0.0], 1500.0);
        let order: Vec<u32> = east.iter().map(|(edge, _)| edge.door_ref).collect();
        assert_eq!(order, [4, 1, 2]);
        // A standing player (no heading) ranks by distance alone.
        let still = graph.rank_next_doors(TAMRIEL, [0.0; 3], [0.0, 0.0], 1500.0);
        let order: Vec<u32> = still.iter().map(|(edge, _)| edge.door_ref).collect();
        assert_eq!(order, [4, 1, 2]);
    }

    #[test]
    fn summary_counts_interiors_with_several_doors() {
        let summary = fixture().summary();
        assert_eq!(
            summary,
            PortalGraphSummary {
                spaces: 4,
                doors: 8,
                links_into_interiors: 5,
                interiors_reached: 3,
                interiors_with_several_doors: 1,
                unresolved: 1,
            }
        );
    }

    #[test]
    fn reads_the_graph_from_door_links_and_references() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                r#"CREATE TABLE cells(id INTEGER PRIMARY KEY,worldspace_id INTEGER);
                CREATE TABLE "references"(id INTEGER PRIMARY KEY,cell_id INTEGER NOT NULL,
                    pos_x REAL NOT NULL,pos_y REAL NOT NULL,pos_z REAL NOT NULL);
                CREATE TABLE door_links(ref_id INTEGER PRIMARY KEY,destination_ref_id INTEGER NOT NULL,
                    pos_x REAL NOT NULL,pos_y REAL NOT NULL,pos_z REAL NOT NULL,
                    rot_x REAL NOT NULL,rot_y REAL NOT NULL,rot_z REAL NOT NULL,
                    destination_cell_id INTEGER,destination_worldspace_id INTEGER);
                INSERT INTO cells VALUES(5,60);
                INSERT INTO cells VALUES(100,NULL);
                INSERT INTO "references" VALUES(1,5,10,20,30);
                INSERT INTO "references" VALUES(2,100,1,2,3);
                INSERT INTO door_links VALUES(1,2,1,2,3,0,0,1.5,100,NULL);
                INSERT INTO door_links VALUES(2,1,10,20,30,0,0,0.5,5,60);"#,
            )
            .unwrap();
        let graph = PortalGraph::read(&connection).unwrap();
        assert_eq!(
            graph.door(1),
            Some(&PortalEdge {
                door_ref: 1,
                from: Space::Worldspace(60),
                position: [10.0, 20.0, 30.0],
                destination_ref: 2,
                to: Some(Space::Interior(100)),
                arrival_position: [1.0, 2.0, 3.0],
                arrival_rotation: [0.0, 0.0, 1.5],
            })
        );
        assert_eq!(graph.door(2).unwrap().from, Space::Interior(100));
        assert_eq!(graph.door(2).unwrap().to, Some(Space::Worldspace(60)));
    }

    #[test]
    fn a_database_without_door_links_is_an_empty_graph() {
        let connection = Connection::open_in_memory().unwrap();
        let graph = PortalGraph::read(&connection).unwrap();
        assert_eq!(graph.summary(), PortalGraphSummary::default());
    }
}
