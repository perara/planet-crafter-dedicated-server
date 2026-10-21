//! Conservative aerial clearance. Docking shafts may cross walls; cruise never crosses bounds.
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use crate::idmap::FxHashMap;

type V3 = [f32; 3];
const CELL: f32 = 64.0;
const CLEARANCE: f32 = 3.0;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Bounds {
    pub min: V3,
    pub max: V3,
}
impl Bounds {
    pub fn transformed(&self, position: V3, rotation: [f32; 4]) -> Self {
        let rotation = if rotation == [0.0; 4] { [0.0, 0.0, 0.0, 1.0] } else { rotation };
        let center = [0, 1, 2].map(|i| (self.min[i] + self.max[i]) * 0.5);
        let half = [0, 1, 2].map(|i| (self.max[i] - self.min[i]) * 0.5);
        let center = super::rotate(rotation, center);
        let axes = [0, 1, 2].map(|j| {
            let mut v = [0.0; 3];
            v[j] = half[j];
            super::rotate(rotation, v)
        });
        let half = [0, 1, 2].map(|i| axes.iter().map(|a| a[i].abs()).sum::<f32>());
        Self {
            min: [0, 1, 2].map(|i| position[i] + center[i] - half[i]),
            max: [0, 1, 2].map(|i| position[i] + center[i] + half[i]),
        }
    }
    fn valid(&self) -> bool {
        (0..3).all(|i| self.min[i].is_finite() && self.max[i].is_finite() && self.min[i] <= self.max[i] && self.min[i].abs().max(self.max[i].abs()) < 1e6)
    }
    fn intersects(&self, a: V3, b: V3, axes: &[usize]) -> bool {
        let (mut near, mut far) = (0.0f32, 1.0f32);
        for &i in axes {
            let (low, high, delta) = (self.min[i] - CLEARANCE, self.max[i] + CLEARANCE, b[i] - a[i]);
            if delta.abs() < 1e-6 {
                if a[i] < low || a[i] > high {
                    return false;
                }
            } else {
                let (x, y) = ((low - a[i]) / delta, (high - a[i]) / delta);
                near = near.max(x.min(y));
                far = far.min(x.max(y));
                if near > far {
                    return false;
                }
            }
        }
        true
    }
}

#[derive(Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Data {
    pub format: u32,
    pub build_id: String,
    /// Shared with the terrain grids built from them, which would each copy the 18 MB.
    pub planets: HashMap<i32, Arc<[Bounds]>>,
    pub buildings: HashMap<String, Vec<Bounds>>,
}
impl Data {
    pub fn validate(&self, build: &str) -> Result<(), String> {
        if self.format != 1 || self.build_id != build.trim() {
            return Err("drone collision map format or game build differs; regenerate with pcds drone-map".into());
        }
        if !self.planets.values().map(AsRef::as_ref).chain(self.buildings.values().map(Vec::as_slice)).flatten().all(Bounds::valid) {
            return Err("drone collision map contains invalid bounds".into());
        }
        Ok(())
    }
}

#[derive(Default)]
pub struct Grid {
    bounds: Arc<[Bounds]>,
    cells: FxHashMap<(i32, i32), std::ops::Range<usize>>,
    indices: Box<[usize]>,
    large: Vec<usize>,
    visits: Vec<u32>,
    query: u32,
}
fn cell(x: f32) -> i32 {
    (x / CELL).floor() as i32
}
impl Grid {
    pub fn new(bounds: impl Into<Arc<[Bounds]>>) -> Self {
        let bounds = bounds.into();
        let mut grid = Self { visits: vec![0; bounds.len()], bounds, ..Default::default() };
        let cells = |b: &Bounds| {
            let (x0, x1, z0, z1) = (
                cell(b.min[0] - CLEARANCE),
                cell(b.max[0] + CLEARANCE),
                cell(b.min[2] - CLEARANCE),
                cell(b.max[2] + CLEARANCE),
            );
            // Dynamic bounds can exceed the static map's limits (e.g. a client's rotation).
            let (nx, nz) = (i64::from(x1) - i64::from(x0) + 1, i64::from(z1) - i64::from(z0) + 1);
            if nx > 4096 || nz > 4096 || nx * nz > 4096 { None } else { Some((x0..=x1, z0..=z1)) }
        };
        // Count first: each cell gets an exact slice in one immutable allocation.
        for (i, b) in grid.bounds.iter().enumerate() {
            let Some((xs, zs)) = cells(b) else { grid.large.push(i); continue };
            for x in xs {
                for z in zs.clone() {
                    grid.cells.entry((x, z)).or_insert(0..0).end += 1;
                }
            }
        }
        let mut count = 0;
        for range in grid.cells.values_mut() {
            let end = count + range.end;
            *range = count..count;
            count = end;
        }
        grid.indices = vec![0; count].into_boxed_slice();
        // Fill in the original bounds order; end is the write cursor until this is done.
        for (i, b) in grid.bounds.iter().enumerate() {
            let Some((xs, zs)) = cells(b) else { continue };
            for x in xs {
                for z in zs.clone() {
                    let range = grid.cells.get_mut(&(x, z)).unwrap();
                    grid.indices[range.end] = i;
                    range.end += 1;
                }
            }
        }
        grid
    }
    fn candidates(&mut self, a: V3, b: V3, mut visit: impl FnMut(&Bounds)) {
        let (mut x, mut z, end_x, end_z) = (cell(a[0]), cell(a[2]), cell(b[0]), cell(b[2]));
        if i64::from(end_x).abs_diff(i64::from(x)) + i64::from(end_z).abs_diff(i64::from(z)) > 4096 {
            self.bounds.iter().for_each(visit);
            return;
        }
        self.query = self.query.wrapping_add(1);
        if self.query == 0 {
            self.visits.fill(0);
            self.query = 1;
        }
        let mut collect = |items: &[usize]| {
            for &i in items {
                if self.visits[i] != self.query {
                    self.visits[i] = self.query;
                    visit(&self.bounds[i]);
                }
            }
        };
        collect(&self.large);
        let (dx, dz) = ((end_x - x).signum(), (end_z - z).signum());
        let crossing = |axis: usize, c: i32, step: i32| {
            if step == 0 { f64::INFINITY } else {
                (f64::from(c + i32::from(step > 0)) * f64::from(CELL) - f64::from(a[axis])) / (f64::from(b[axis]) - f64::from(a[axis]))
            }
        };
        loop {
            if let Some(v) = self.cells.get(&(x, z)) { collect(&self.indices[v.clone()]); }
            if x == end_x && z == end_z { break; }
            let (tx, tz) = (if x == end_x { f64::INFINITY } else { crossing(0, x, dx) }, if z == end_z { f64::INFINITY } else { crossing(2, z, dz) });
            // Bounds are already padded in the index; only crossed cells are needed.
            // Include both side cells at a corner, even when floating point rounds the tie.
            if (tx - tz).abs() <= 1e-10 {
                if let Some(v) = self.cells.get(&(x + dx, z)) { collect(&self.indices[v.clone()]); }
                if let Some(v) = self.cells.get(&(x, z + dz)) { collect(&self.indices[v.clone()]); }
                x += dx;
                z += dz;
            } else if tx < tz {
                x += dx;
            } else {
                z += dz;
            }
        }
    }
    // shortcut: collider bounds overestimate some mountains; use mesh-level routing if detours dominate.
    pub fn route_with(&mut self, other: Option<&mut Self>, from: V3, to: V3) -> Vec<V3> {
        let (blocked, height) = self.clearance(from, to);
        let (other_blocked, other_height) = other.map(|g| g.clearance(from, to)).unwrap_or((false, from[1].max(to[1])));
        if !blocked && !other_blocked {
            return vec![to];
        }
        let height = height.max(other_height);
        vec![[from[0], height, from[2]], [to[0], height, to[2]], to]
    }

    fn clearance(&mut self, from: V3, to: V3) -> (bool, f32) {
        let mut blocked = false;
        let mut height = from[1].max(to[1]);
        self.candidates(from, to, |b| {
            if b.intersects(from, to, &[0, 2]) {
                height = height.max(b.max[1] + CLEARANCE + 0.1);
                blocked |= b.intersects(from, to, &[0, 1, 2]);
            }
        });
        (blocked, height)
    }

    #[cfg(test)]
    pub fn route(&mut self, from: V3, to: V3) -> Vec<V3> {
        self.route_with(None, from, to)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires the installed game's private drone-navigation.json"]
    fn extracted_map_routes_clear_every_planet_and_building_prefab() {
        let dir = std::path::Path::new(crate::test_data().expect("game tables required"));
        let data: Data = serde_json::from_slice(&std::fs::read(dir.join("drone-navigation.json")).unwrap()).unwrap();
        data.validate(&std::fs::read_to_string(dir.join("buildid")).unwrap()).unwrap();
        assert!(!data.planets.is_empty());
        let mut routes = 0;
        for (planet, bounds) in &data.planets {
            let mut grid = Grid::new(bounds.clone());
            let candidates: Vec<_> = bounds.iter().filter(|b| {
                b.max[1] > 10.0 && b.max[1] < 300.0 && (2.0..100.0).contains(&(b.max[0] - b.min[0]))
                    && (2.0..100.0).contains(&(b.max[2] - b.min[2]))
            }).collect();
            assert!(candidates.len() >= 10, "planet {planet} has no representative terrain");
            for b in candidates.iter().step_by((candidates.len() / 10).max(1)).take(10) {
                let x = (b.min[0] + b.max[0]) * 0.5;
                let z = (b.min[2] + b.max[2]) * 0.5;
                let (from, to) = ([x - 80.0, b.max[1] - 2.0, z], [x + 80.0, b.max[1] - 2.0, z]);
                let route = grid.route(from, to);
                assert_eq!(route.len(), 3, "the direct flight intersects the selected terrain");
                // Independent full scan for this axis-aligned flight, without the grid or slab query.
                let highest = bounds.iter().filter(|b| {
                    b.min[0] - 3.0 <= to[0] && b.max[0] + 3.0 >= from[0]
                        && b.min[2] - 3.0 <= z && b.max[2] + 3.0 >= z
                }).map(|b| b.max[1]).fold(f32::NEG_INFINITY, f32::max);
                assert!(route[0][1] > highest + 3.0, "planet {planet}: cruise misses an obstacle");
                assert_eq!(route[0][0], from[0]);
                assert_eq!(route[1], [to[0], route[0][1], z]);
                assert_eq!(route[2], to);
                routes += 1;
            }
        }
        println!("extracted map: {routes} obstructed routes across {} planets cleared an independent full scan", data.planets.len());
        assert!(!data.buildings.is_empty());
        for (name, local) in &data.buildings {
            let bounds: Vec<_> = local.iter().map(|b| b.transformed([2000.0, 500.0, 3000.0], super::super::from_euler_y(90.0))).collect();
            let first = &bounds[0];
            let z = (first.min[2] + first.max[2]) * 0.5;
            let y = (first.min[1] + first.max[1]) * 0.5;
            let from = [bounds.iter().map(|b| b.min[0]).fold(f32::INFINITY, f32::min) - 8.0, y, z];
            let to = [bounds.iter().map(|b| b.max[0]).fold(f32::NEG_INFINITY, f32::max) + 8.0, y, z];
            let highest = bounds.iter().filter(|b| b.min[2] - 3.0 <= z && b.max[2] + 3.0 >= z)
                .map(|b| b.max[1]).fold(f32::NEG_INFINITY, f32::max);
            let route = Grid::new(Vec::new()).route_with(Some(&mut Grid::new(bounds)), from, to);
            assert_eq!(route.len(), 3, "{name}: an obstructed building-only route must detour");
            assert!(route[0][1] > highest + 3.0, "{name}: transformed building crossed");
        }
        println!("extracted map: {} rotated building prefabs cleared with no terrain", data.buildings.len());
    }
    #[test]
    fn extreme_dynamic_bounds_cannot_wrap_the_cell_count() {
        let bounds = Bounds { min: [-1.0; 3], max: [1.0; 3] }
            .transformed([0.0; 3], [1e9, 0.0, 0.0, 1e9]);
        assert!(bounds.min.iter().chain(&bounds.max).all(|v| v.is_finite()));
        let mut grid = Grid::new(vec![bounds]);
        assert!(grid.cells.is_empty());
        assert!(grid.indices.is_empty());
        assert_eq!(grid.large, vec![0]);
        let route = grid.route([0.0, 5.0, 0.0], [60.0, 5.0, 60.0]);
        assert!(route.iter().flatten().all(|v| v.is_finite()));
    }

    #[test]
    fn terrain_bounds_are_shared_and_map_json_is_unchanged() {
        let text = r#"{"format":1,"build_id":"abc","planets":{"1":[{"min":[10.0,0.0,-4.0],"max":[20.0,40.0,4.0]}]},"buildings":{}}"#;
        let data: Data = serde_json::from_str(text).unwrap();
        let bounds = &data.planets[&1];
        let mut shared = Grid::new(bounds.clone());
        assert!(Arc::ptr_eq(bounds, &shared.bounds));
        let mut copied = Grid::new(bounds.to_vec());
        assert_eq!(shared.route([0.0, 5.0, 0.0], [60.0, 5.0, 0.0]), copied.route([0.0, 5.0, 0.0], [60.0, 5.0, 0.0]));
        assert_eq!(serde_json::to_value(&data).unwrap(), serde_json::from_str::<serde_json::Value>(text).unwrap());
    }
    #[test]
    fn cruise_clears_all_bounds_and_docking_is_vertical() {
        let bounds = vec![
            Bounds {
                min: [10.0, 0.0, -4.0],
                max: [20.0, 40.0, 4.0],
            },
            Bounds {
                min: [40.0, 0.0, -2.0],
                max: [45.0, 60.0, 2.0],
            },
        ];
        let mut grid = Grid::new(bounds);
        let route = grid.route([0.0, 5.0, 0.0], [60.0, 5.0, 0.0]);
        assert_eq!(route.len(), 3);
        assert!(route[0][1] > 63.0);
        assert_eq!(route[0][0], 0.0);
        assert_eq!(route[1][0], 60.0);
        assert!(grid.bounds.iter().all(|b| !b.intersects(route[0], route[1], &[0, 1, 2])));
        assert_eq!(grid.route([0.0, 100.0, 0.0], [60.0, 100.0, 0.0]).len(), 1);
        assert_eq!(grid.route([0.0, 5.0, 100.0], [60.0, 5.0, 100.0]).len(), 1);
    }
    #[test]
    fn terrain_detours_also_clear_overhead_buildings() {
        let mut terrain = Grid::new(vec![Bounds {
            min: [10.0, 0.0, -5.0],
            max: [20.0, 40.0, 5.0],
        }]);
        let mut overhead = Grid::new(vec![Bounds {
            min: [30.0, 35.0, -5.0],
            max: [40.0, 80.0, 5.0],
        }]);
        let (from, to) = ([0.0, 5.0, 0.0], [60.0, 5.0, 0.0]);
        assert_eq!(overhead.route(from, to), vec![to], "the original low route passes below the building");
        let route = terrain.route_with(Some(&mut overhead), from, to);
        assert!(route[0][1] > 83.0, "the raised cruise also must clear overhead geometry");
        assert!(terrain.bounds.iter().chain(overhead.bounds.iter()).all(|b| !b.intersects(route[0], route[1], &[0, 1, 2])));
    }

    #[test]
    fn indexed_routes_match_full_scan_across_negative_cells_and_long_diagonals() {
        let bounds: Vec<_> = (0..5000)
            .map(|i| {
                let x = (i % 100) as f32 * 50.0 - 2500.0;
                let z = (i / 100) as f32 * 50.0 - 1250.0;
                Bounds {
                    min: [x, 0.0, z],
                    max: [x + 5.0, (i % 70) as f32, z + 5.0],
                }
            })
            .collect();
        let mut grid = Grid::new(bounds.clone());
        let mut full = Grid::new(bounds);
        full.cells.clear();
        full.large = (0..5000).collect();
        for i in 0..50 {
            let a = [-3000.0, 10.0, i as f32 * 63.0 - 1600.0];
            let b = [3000.0, 10.0, 1600.0 - i as f32 * 63.0];
            assert_eq!(grid.route(a, b), full.route(a, b));
        }
        let mut candidates = 0;
        grid.candidates([-100.0, 2.0, -100.0], [0.0, 2.0, 0.0], |_| candidates += 1);
        assert!(candidates < 100);
    }

    #[test]
    fn crossed_cells_match_full_scan_at_edges_corners_and_in_both_directions() {
        let bounds: Vec<_> = (-12..=12).flat_map(|x| (-12..=12).map(move |z| Bounds {
            min: [x as f32 * CELL - CLEARANCE, -10.0, z as f32 * CELL - CLEARANCE],
            max: [x as f32 * CELL + CLEARANCE, (x * x + z * z) as f32, z as f32 * CELL + CLEARANCE],
        })).chain(std::iter::once(Bounds { min: [-10000.0, -20.0, -10000.0], max: [10000.0, -10.0, 10000.0] })).collect();
        let mut grid = Grid::new(bounds.clone());
        let mut full = Grid::new(bounds);
        full.cells.clear();
        full.large = (0..full.bounds.len()).collect();
        let mut rng = crate::game::Rng::fixed(81);
        for i in 0..1000 {
            let a = [rng.range(-1000, 1000) as f32, rng.range(-20, 400) as f32, rng.range(-1000, 1000) as f32];
            let mut b = [rng.range(-1000, 1000) as f32, rng.range(-20, 400) as f32, rng.range(-1000, 1000) as f32];
            match i % 5 {
                0 => b = a,
                1 => b[0] = a[0],
                2 => b[2] = a[2],
                3 => b = [a[2], b[1], a[0]],
                _ => {}
            }
            assert_eq!(grid.route(a, b), full.route(a, b), "{a:?} -> {b:?}");
            assert_eq!(grid.route(b, a), full.route(b, a), "{b:?} -> {a:?}");
        }
        for (a, b) in [
            ([-1024.0, 5.0, -1024.0], [1024.0, 5.0, 1024.0]),
            ([64.0, 5.0, -1024.0], [64.0, 5.0, 1024.0]),
            ([-1024.0, 5.0, 64.0], [1024.0, 5.0, 64.0]),
            ([-200000.0, 5.0, 0.0], [200000.0, 5.0, 0.0]),
        ] {
            assert_eq!(grid.route(a, b), full.route(a, b));
            assert_eq!(grid.route(b, a), full.route(b, a));
        }
        grid.query = u32::MAX;
        grid.visits.fill(1);
        let (a, b) = ([-500.0, 5.0, 0.0], [500.0, 5.0, 0.0]);
        assert_eq!(grid.route(a, b), full.route(a, b), "visit stamps reset when the counter wraps");
    }
    #[test]
    fn invalid_or_stale_maps_are_refused() {
        let mut d = Data {
            format: 1,
            build_id: "abc".into(),
            ..Default::default()
        };
        assert!(d.validate("abc\n").is_ok());
        assert!(d.validate("xyz").is_err());
        d.planets.insert(
            1,
            vec![Bounds {
                min: [f32::NAN; 3],
                max: [0.0; 3],
            }]
            .into(),
        );
        assert!(d.validate("abc").is_err());
    }
}
